use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Expr, FnArg, ItemFn, Lit, MetaNameValue, Pat, ReturnType, Token, Type, parse_macro_input,
    punctuated::Punctuated,
};

#[proc_macro_attribute]
pub fn tool(attributes: TokenStream, item: TokenStream) -> TokenStream {
    let attributes = parse_macro_input!(
        attributes with Punctuated::<MetaNameValue, Token![,]>::parse_terminated
    );
    let function = parse_macro_input!(item as ItemFn);

    expand_tool(attributes, function)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_tool(
    attributes: Punctuated<MetaNameValue, Token![,]>,
    function: ItemFn,
) -> syn::Result<TokenStream2> {
    if function.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            function.sig.fn_token,
            "#[tool] requires an async function",
        ));
    }
    if !function.sig.generics.params.is_empty() || function.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &function.sig.generics,
            "#[tool] does not support generic functions",
        ));
    }
    if matches!(function.sig.output, ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "#[tool] function must return Result<T, E>",
        ));
    }

    let mut configured_name = None;
    let mut description = None;
    for attribute in attributes {
        let Some(key) = attribute.path.get_ident() else {
            return Err(syn::Error::new_spanned(
                attribute.path,
                "unsupported attribute",
            ));
        };
        let Expr::Lit(expression) = attribute.value else {
            return Err(syn::Error::new_spanned(
                attribute,
                "attribute value must be a string literal",
            ));
        };
        let Lit::Str(value) = expression.lit else {
            return Err(syn::Error::new_spanned(
                expression,
                "attribute value must be a string literal",
            ));
        };
        match key.to_string().as_str() {
            "name" => configured_name = Some(value),
            "description" => description = Some(value),
            _ => {
                return Err(syn::Error::new_spanned(
                    key,
                    "supported attributes are `name` and `description`",
                ));
            }
        }
    }

    let runtime = runtime_crate();
    let visibility = &function.vis;
    let function_name = &function.sig.ident;
    let implementation_name = format_ident!("__g_tool_impl_{function_name}");
    let tool_name = configured_name
        .map(|name| name.value())
        .unwrap_or_else(|| function_name.to_string());
    // Without an explicit `description`, fall back to the function's doc comment.
    let description = description
        .map(|value| value.value())
        .unwrap_or_else(|| doc_comment(&function.attrs));
    let attributes = &function.attrs;
    let block = &function.block;
    let mut implementation_signature = function.sig.clone();
    implementation_signature.ident = implementation_name.clone();

    // Every parameter, in order, as passed to the implementation.
    let mut call_arguments = Vec::new();
    let mut context_argument = None;
    let mut argument_names = Vec::new();
    let mut argument_types = Vec::new();
    let mut optional_arguments = Vec::new();
    for argument in &function.sig.inputs {
        let FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new_spanned(
                argument,
                "#[tool] does not support methods with a self receiver",
            ));
        };
        let Pat::Ident(pattern) = argument.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &argument.pat,
                "tool parameters must use simple identifier patterns",
            ));
        };
        if is_tool_context(&argument.ty) {
            if context_argument.is_some() {
                return Err(syn::Error::new_spanned(
                    argument,
                    "#[tool] accepts at most one ToolContext parameter",
                ));
            }
            context_argument = Some(pattern.ident.clone());
            call_arguments.push(quote!(context));
            continue;
        }
        let name = pattern.ident.clone();
        call_arguments.push(quote!(#name));
        argument_names.push(name);
        argument_types.push(argument.ty.as_ref().clone());
        optional_arguments.push(is_option(&argument.ty));
    }
    let context_binding = if context_argument.is_some() {
        quote!(context)
    } else {
        quote!(_context)
    };

    let schema_properties = argument_names
        .iter()
        .zip(&argument_types)
        .map(|(name, ty)| {
            let name = name.to_string();
            // One generator for all parameters, so shared types land in a single
            // root-level `$defs` that their `$ref`s resolve against.
            quote! {
                properties.insert(
                    #name.to_owned(),
                    #runtime::__private::serde_json::to_value(generator.subschema_for::<#ty>())
                        .expect("JSON Schema must be serializable"),
                );
            }
        });
    let required_arguments = argument_names
        .iter()
        .zip(&optional_arguments)
        .filter(|(_, optional)| !**optional)
        .map(|(name, _)| name.to_string());
    let deserialize_arguments = argument_names
        .iter()
        .zip(&argument_types)
        .zip(&optional_arguments)
        .map(|((name, ty), optional)| {
            let field = name.to_string();
            let value = if *optional {
                quote! {
                    arguments.get(#field).cloned().unwrap_or(
                        #runtime::__private::serde_json::Value::Null
                    )
                }
            } else {
                quote! {
                    arguments.get(#field).cloned().ok_or_else(||
                        #runtime::ToolError::new(format!("missing required argument `{}`", #field))
                    )?
                }
            };
            quote! {
                let #name: #ty = #runtime::__private::serde_json::from_value(#value)
                    .map_err(|error| #runtime::ToolError::new(format!(
                        "invalid argument `{}`: {}", #field, error
                    )))?;
            }
        });

    Ok(quote! {
        #(#attributes)*
        #implementation_signature #block

        #[allow(non_camel_case_types)]
        #[doc = #description]
        #visibility struct #function_name;

        #[#runtime::__private::async_trait::async_trait]
        impl #runtime::Tool for #function_name {
            fn spec(&self) -> #runtime::ToolSpec {
                // The schema is fixed at compile time; generate it once.
                static SPEC: ::std::sync::LazyLock<#runtime::ToolSpec> =
                    ::std::sync::LazyLock::new(|| {
                        let mut generator =
                            #runtime::__private::schemars::generate::SchemaGenerator::default();
                        let mut properties = #runtime::__private::serde_json::Map::new();
                        #(#schema_properties)*
                        let mut input_schema = #runtime::__private::serde_json::json!({
                            "type": "object",
                            "properties": properties,
                            "required": [#(#required_arguments),*],
                            "additionalProperties": false
                        });
                        let definitions = generator.take_definitions(true);
                        if !definitions.is_empty() {
                            input_schema["$defs"] =
                                #runtime::__private::serde_json::Value::Object(definitions);
                        }
                        #runtime::ToolSpec {
                            name: #tool_name.to_owned(),
                            description: #description.to_owned(),
                            input_schema,
                            behavior: #runtime::ToolBehavior::default(),
                        }
                    });
                SPEC.clone()
            }

            async fn call(
                &self,
                #context_binding: #runtime::ToolContext,
                input: #runtime::__private::serde_json::Value,
            ) -> Result<#runtime::__private::serde_json::Value, #runtime::ToolError> {
                let arguments = input.as_object().ok_or_else(||
                    #runtime::ToolError::new("tool arguments must be a JSON object")
                )?;
                #(#deserialize_arguments)*
                let output = #implementation_name(#(#call_arguments),*)
                    .await
                    .map_err(|error| #runtime::ToolError::new(error.to_string()))?;
                #runtime::__private::serde_json::to_value(output)
                    .map_err(|error| #runtime::ToolError::new(format!(
                        "failed to serialize tool output: {}", error
                    )))
            }
        }
    })
}

fn doc_comment(attributes: &[syn::Attribute]) -> String {
    attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("doc"))
        .filter_map(|attribute| match &attribute.meta {
            syn::Meta::NameValue(MetaNameValue {
                value:
                    Expr::Lit(syn::ExprLit {
                        lit: Lit::Str(text),
                        ..
                    }),
                ..
            }) => Some(text.value().trim().to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn is_tool_context(ty: &Type) -> bool {
    last_segment_is(ty, "ToolContext")
}

fn is_option(ty: &Type) -> bool {
    last_segment_is(ty, "Option")
}

fn last_segment_is(ty: &Type, name: &str) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == name)
}

fn runtime_crate() -> TokenStream2 {
    match crate_name("g") {
        Ok(FoundCrate::Name(name)) => {
            let name = format_ident!("{name}");
            quote!(::#name)
        }
        Ok(FoundCrate::Itself) | Err(_) => quote!(::g),
    }
}
