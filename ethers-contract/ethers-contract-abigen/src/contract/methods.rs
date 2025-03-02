use std::collections::{btree_map::Entry, BTreeMap};

use anyhow::{Context as _, Result};
use inflector::Inflector;
use proc_macro2::{Literal, TokenStream};
use quote::quote;
use syn::Ident;

use ethers_core::{
    abi::{Function, FunctionExt, Param, ParamType},
    macros::{ethers_contract_crate, ethers_core_crate},
    types::Selector,
};

use super::{types, util, Context};

/// The maximum amount of overloaded functions that are attempted to auto aliased with their param
/// name. If there is a function that with `NAME_ALIASING_OVERLOADED_FUNCTIONS_CAP` overloads then
/// all functions are aliased with their index, like `log0, log1, log2,....`
const NAME_ALIASING_OVERLOADED_FUNCTIONS_CAP: usize = 3;

/// Expands a context into a method struct containing all the generated bindings
/// to the Solidity contract methods.
impl Context {
    /// Expands all method implementations
    pub(crate) fn methods_and_call_structs(&self) -> Result<(TokenStream, TokenStream)> {
        let aliases = self.get_method_aliases()?;
        let sorted_functions: BTreeMap<_, _> = self.abi.functions.iter().collect();
        let functions = sorted_functions
            .values()
            .map(std::ops::Deref::deref)
            .flatten()
            .map(|function| {
                let signature = function.abi_signature();
                self.expand_function(function, aliases.get(&signature).cloned())
                    .with_context(|| format!("error expanding function '{}'", signature))
            })
            .collect::<Result<Vec<_>>>()?;

        let function_impls = quote! { #( #functions )* };
        let call_structs = self.expand_call_structs(aliases)?;

        Ok((function_impls, call_structs))
    }

    /// Expands to the corresponding struct type based on the inputs of the given function
    fn expand_call_struct(
        &self,
        function: &Function,
        alias: Option<&Ident>,
    ) -> Result<TokenStream> {
        let call_name = expand_call_struct_name(function, alias);
        let fields = self.expand_input_pairs(function)?;
        // expand as a tuple if all fields are anonymous
        let all_anonymous_fields = function.inputs.iter().all(|input| input.name.is_empty());
        let call_type_definition = if all_anonymous_fields {
            // expand to a tuple struct
            expand_data_tuple(&call_name, &fields)
        } else {
            // expand to a struct
            expand_data_struct(&call_name, &fields)
        };
        let function_name = &function.name;
        let abi_signature = function.abi_signature();
        let doc = format!(
            "Container type for all input parameters for the `{}`function with signature `{}` and selector `{:?}`",
            function.name,
            abi_signature,
            function.selector()
        );
        let abi_signature_doc = util::expand_doc(&doc);
        let ethers_contract = ethers_contract_crate();
        // use the same derives as for events
        let derives = util::expand_derives(&self.event_derives);

        Ok(quote! {
            #abi_signature_doc
            #[derive(Clone, Debug, Default, Eq, PartialEq, #ethers_contract::EthCall, #ethers_contract::EthDisplay, #derives)]
            #[ethcall( name = #function_name, abi = #abi_signature )]
            pub #call_type_definition
        })
    }

    /// Expands all structs
    fn expand_call_structs(&self, aliases: BTreeMap<String, Ident>) -> Result<TokenStream> {
        let mut struct_defs = Vec::new();
        let mut struct_names = Vec::new();
        let mut variant_names = Vec::new();
        for function in self.abi.functions.values().flatten() {
            let signature = function.abi_signature();
            let alias = aliases.get(&signature);
            struct_defs.push(self.expand_call_struct(function, alias)?);
            struct_names.push(expand_call_struct_name(function, alias));
            variant_names.push(expand_call_struct_variant_name(function, alias));
        }

        let struct_def_tokens = quote! {
            #(#struct_defs)*
        };

        if struct_defs.len() <= 1 {
            // no need for an enum
            return Ok(struct_def_tokens)
        }

        let ethers_core = ethers_core_crate();
        let ethers_contract = ethers_contract_crate();

        let enum_name = self.expand_calls_enum_name();
        Ok(quote! {
            #struct_def_tokens

           #[derive(Debug, Clone, PartialEq, Eq, #ethers_contract::EthAbiType)]
            pub enum #enum_name {
                #(#variant_names(#struct_names)),*
            }

        impl  #ethers_core::abi::AbiDecode for #enum_name {
            fn decode(data: impl AsRef<[u8]>) -> Result<Self, #ethers_core::abi::AbiError> {
                 #(
                    if let Ok(decoded) = <#struct_names as #ethers_core::abi::AbiDecode>::decode(data.as_ref()) {
                        return Ok(#enum_name::#variant_names(decoded))
                    }
                )*
                Err(#ethers_core::abi::Error::InvalidData.into())
            }
        }

         impl  #ethers_core::abi::AbiEncode for #enum_name {
            fn encode(self) -> Vec<u8> {
                match self {
                    #(
                        #enum_name::#variant_names(element) => element.encode()
                    ),*
                }
            }
        }

        impl ::std::fmt::Display for #enum_name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                match self {
                    #(
                        #enum_name::#variant_names(element) => element.fmt(f)
                    ),*
                }
            }
        }

        #(
            impl ::std::convert::From<#struct_names> for #enum_name {
                fn from(var: #struct_names) -> Self {
                    #enum_name::#variant_names(var)
                }
            }
        )*

        })
    }

    /// The name ident of the calls enum
    fn expand_calls_enum_name(&self) -> Ident {
        util::ident(&format!("{}Calls", self.contract_name))
    }

    /// Expands to the `name : type` pairs of the function's inputs
    fn expand_input_pairs(&self, fun: &Function) -> Result<Vec<(TokenStream, TokenStream)>> {
        let mut args = Vec::with_capacity(fun.inputs.len());
        for (idx, param) in fun.inputs.iter().enumerate() {
            let name = util::expand_input_name(idx, &param.name);
            let ty = self.expand_input_param(fun, &param.name, &param.kind)?;
            args.push((name, ty));
        }
        Ok(args)
    }

    /// Expands the arguments for the call that eventually calls the contract
    fn expand_contract_call_args(&self, fun: &Function) -> Result<TokenStream> {
        let mut call_args = Vec::with_capacity(fun.inputs.len());
        for (idx, param) in fun.inputs.iter().enumerate() {
            let name = util::expand_input_name(idx, &param.name);
            let call_arg = match param.kind {
                // this is awkward edge case where the function inputs are a single struct
                // we need to force this argument into a tuple so it gets expanded to `((#name,))`
                // this is currently necessary because internally `flatten_tokens` is called which
                // removes the outermost `tuple` level and since `((#name))` is not
                // a rust tuple it doesn't get wrapped into another tuple that will be peeled off by
                // `flatten_tokens`
                ParamType::Tuple(_) if fun.inputs.len() == 1 => {
                    // make sure the tuple gets converted to `Token::Tuple`
                    quote! {(#name,)}
                }
                _ => name,
            };
            call_args.push(call_arg);
        }
        let call_args = match call_args.len() {
            0 => quote! { () },
            1 => quote! { #( #call_args )* },
            _ => quote! { ( #(#call_args, )* ) },
        };

        Ok(call_args)
    }

    fn expand_input_param(
        &self,
        fun: &Function,
        param: &str,
        kind: &ParamType,
    ) -> Result<TokenStream> {
        match kind {
            ParamType::Array(ty) => {
                let ty = self.expand_input_param(fun, param, ty)?;
                Ok(quote! {
                    ::std::vec::Vec<#ty>
                })
            }
            ParamType::FixedArray(ty, size) => {
                let ty = self.expand_input_param(fun, param, ty)?;
                let size = *size;
                Ok(quote! {[#ty; #size]})
            }
            ParamType::Tuple(_) => {
                let ty = if let Some(rust_struct_name) =
                    self.internal_structs.get_function_input_struct_type(&fun.name, param)
                {
                    let ident = util::ident(rust_struct_name);
                    quote! {#ident}
                } else {
                    types::expand(kind)?
                };
                Ok(ty)
            }
            _ => types::expand(kind),
        }
    }

    /// Expands a single function with the given alias
    fn expand_function(&self, function: &Function, alias: Option<Ident>) -> Result<TokenStream> {
        let name = expand_function_name(function, alias.as_ref());
        let selector = expand_selector(function.selector());

        // TODO use structs
        let outputs = expand_fn_outputs(&function.outputs)?;

        let ethers_contract = ethers_contract_crate();

        let result = quote! { #ethers_contract::builders::ContractCall<M, #outputs> };

        let contract_args = self.expand_contract_call_args(function)?;
        let function_params =
            self.expand_input_pairs(function)?.into_iter().map(|(name, ty)| quote! { #name: #ty });
        let function_params = quote! { #( , #function_params )* };

        let doc = util::expand_doc(&format!(
            "Calls the contract's `{}` (0x{}) function",
            function.name,
            hex::encode(function.selector())
        ));
        Ok(quote! {

            #doc
            pub fn #name(&self #function_params) -> #result {
                self.0.method_hash(#selector, #contract_args)
                    .expect("method not found (this should never happen)")
            }
        })
    }

    /// Returns the method aliases, either configured by the user or determined
    /// based on overloaded functions.
    ///
    /// In case of overloaded functions we would follow rust's general
    /// convention of suffixing the function name with _with
    // The first function or the function with the least amount of arguments should
    // be named as in the ABI, the following functions suffixed with _with_ +
    // additional_params[0].name + (_and_(additional_params[1+i].name))*
    fn get_method_aliases(&self) -> Result<BTreeMap<String, Ident>> {
        let mut aliases = self.method_aliases.clone();
        // find all duplicates, where no aliases where provided
        for functions in self.abi.functions.values() {
            if functions.iter().filter(|f| !aliases.contains_key(&f.abi_signature())).count() <= 1 {
                // no overloads, hence no conflicts
                continue
            }

            let num_functions = functions.len();
            // sort functions by number of inputs asc
            let mut functions = functions.iter().enumerate().collect::<Vec<_>>();
            functions.sort_by(|(_, f1), (_, f2)| f1.inputs.len().cmp(&f2.inputs.len()));

            // the `functions` are now mapped with their index according as defined in the ABI, but
            // we always want the zero arg function (`log()`) to be `log0`, even if it defined after
            // an overloaded function like `log(address)`
            if num_functions > NAME_ALIASING_OVERLOADED_FUNCTIONS_CAP {
                // lots of overloads, so we set `log()` to index 0, and shift all fun
                for (idx, _) in &mut functions[1..] {
                    *idx += 1;
                }
                functions[0].0 = 0;
            } else {
                // for few overloads we stick entirely to the input len order
                for (idx, (f_idx, _)) in functions.iter_mut().enumerate() {
                    *f_idx = idx;
                }
            }

            // the first function will be the function with the least amount of inputs, like log()
            // and is the baseline for the diff
            let (first_fun_idx, first_fun) = functions[0];

            // assuming here that if there is an overloaded function with nameless params like
            // `log;, log(string); log(string, string)` `log()` it should also be
            // aliased as well with its index to `log0`
            let mut needs_alias_for_first_fun_using_idx = false;

            // all the overloaded functions together with their diffs compare to the `first_fun`
            let mut diffs = Vec::new();

            /// helper function that checks if there are any conflicts due to parameter names
            fn name_conflicts(idx: usize, diffs: &[(usize, Vec<&Param>, &Function)]) -> bool {
                let diff = &diffs.iter().find(|(i, _, _)| *i == idx).expect("diff exists").1;

                for (_, other, _) in diffs.iter().filter(|(i, _, _)| *i != idx) {
                    let (a, b) =
                        if other.len() > diff.len() { (other, diff) } else { (diff, other) };

                    if a.iter()
                        .all(|d| b.iter().any(|o| o.name.to_snake_case() == d.name.to_snake_case()))
                    {
                        return true
                    }
                }
                false
            }

            // compare each overloaded function with the `first_fun`
            for (idx, overloaded_fun) in functions.into_iter().skip(1) {
                // attempt to find diff in the input arguments
                let mut diff = Vec::new();
                let mut same_params = true;
                for (idx, i1) in overloaded_fun.inputs.iter().enumerate() {
                    if first_fun.inputs.iter().all(|i2| i1 != i2) {
                        diff.push(i1);
                        same_params = false;
                    } else {
                        // check for cases like `log(string); log(string, string)` by keep track of
                        // same order
                        if same_params && idx + 1 > first_fun.inputs.len() {
                            diff.push(i1);
                        }
                    }
                }
                diffs.push((idx, diff, overloaded_fun));
            }

            for (idx, diff, overloaded_fun) in &diffs {
                let alias = match diff.len() {
                    0 => {
                        // this should not happen since functions with same name and inputs are
                        // illegal
                        anyhow::bail!(
                            "Function with same name and parameter types defined twice: {}",
                            overloaded_fun.name
                        );
                    }
                    1 => {
                        // single additional input params
                        if diff[0].name.is_empty() ||
                            num_functions > NAME_ALIASING_OVERLOADED_FUNCTIONS_CAP ||
                            name_conflicts(*idx, &diffs)
                        {
                            needs_alias_for_first_fun_using_idx = true;
                            format!("{}{}", overloaded_fun.name.to_snake_case(), idx)
                        } else {
                            format!(
                                "{}_with_{}",
                                overloaded_fun.name.to_snake_case(),
                                diff[0].name.to_snake_case()
                            )
                        }
                    }
                    _ => {
                        if diff.iter().any(|d| d.name.is_empty()) ||
                            num_functions > NAME_ALIASING_OVERLOADED_FUNCTIONS_CAP ||
                            name_conflicts(*idx, &diffs)
                        {
                            needs_alias_for_first_fun_using_idx = true;
                            format!("{}{}", overloaded_fun.name.to_snake_case(), idx)
                        } else {
                            // 1 + n additional input params
                            let and = diff
                                .iter()
                                .skip(1)
                                .map(|i| i.name.to_snake_case())
                                .collect::<Vec<_>>()
                                .join("_and_");
                            format!(
                                "{}_with_{}_and_{}",
                                overloaded_fun.name.to_snake_case(),
                                diff[0].name.to_snake_case(),
                                and
                            )
                        }
                    }
                };
                aliases.insert(overloaded_fun.abi_signature(), util::safe_ident(&alias));
            }

            if needs_alias_for_first_fun_using_idx {
                // insert an alias for the root duplicated call
                let prev_alias = format!("{}{}", first_fun.name.to_snake_case(), first_fun_idx);
                aliases.insert(first_fun.abi_signature(), util::safe_ident(&prev_alias));
            }
        }

        // we have to handle the edge cases with underscore prefix and suffix that would get
        // stripped by Inflector::to_snake_case/pascalCase if there is another function that
        // would collide we manually add an alias for it eg. abi = ["_a(), a(), a_(),
        // _a_()"] will generate identical rust functions
        for (name, functions) in self.abi.functions.iter() {
            if name.starts_with('_') || name.ends_with('_') {
                let ident = name.trim_matches('_').trim_end_matches('_');
                // check for possible collisions after Inflector would remove the underscores
                if self.abi.functions.contains_key(ident) {
                    for function in functions {
                        if let Entry::Vacant(entry) = aliases.entry(function.abi_signature()) {
                            // use the full name as alias
                            entry.insert(util::ident(name.as_str()));
                        }
                    }
                }
            }
        }
        Ok(aliases)
    }
}

fn expand_fn_outputs(outputs: &[Param]) -> Result<TokenStream> {
    match outputs.len() {
        0 => Ok(quote! { () }),
        1 => types::expand(&outputs[0].kind),
        _ => {
            let types = outputs
                .iter()
                .map(|param| types::expand(&param.kind))
                .collect::<Result<Vec<_>>>()?;
            Ok(quote! { (#( #types ),*) })
        }
    }
}

fn expand_selector(selector: Selector) -> TokenStream {
    let bytes = selector.iter().copied().map(Literal::u8_unsuffixed);
    quote! { [#( #bytes ),*] }
}

fn expand_function_name(function: &Function, alias: Option<&Ident>) -> Ident {
    if let Some(alias) = alias {
        // snake_case strips leading and trailing underscores so we simply add them back if the
        // alias starts/ends with underscores
        let alias = alias.to_string();
        let ident = alias.to_snake_case();
        util::ident(&util::preserve_underscore_delim(&ident, &alias))
    } else {
        util::safe_ident(&function.name.to_snake_case())
    }
}

/// Expands to the name of the call struct
fn expand_call_struct_name(function: &Function, alias: Option<&Ident>) -> Ident {
    let name = if let Some(alias) = alias {
        // pascal_case strips leading and trailing underscores so we simply add them back if the
        // alias starts/ends with underscores
        let alias = alias.to_string();
        let ident = alias.to_pascal_case();
        let alias = util::preserve_underscore_delim(&ident, &alias);
        format!("{}Call", alias)
    } else {
        format!("{}Call", function.name.to_pascal_case())
    };
    util::ident(&name)
}

/// Expands to the name of the call struct
fn expand_call_struct_variant_name(function: &Function, alias: Option<&Ident>) -> Ident {
    let name = if let Some(alias) = alias {
        let alias = alias.to_string();
        let ident = alias.to_pascal_case();
        util::preserve_underscore_delim(&ident, &alias)
    } else {
        function.name.to_pascal_case()
    };
    util::ident(&name)
}

/// Expands to the tuple struct definition
fn expand_data_tuple(name: &Ident, params: &[(TokenStream, TokenStream)]) -> TokenStream {
    let fields = params
        .iter()
        .map(|(_, ty)| {
            quote! {
            pub #ty }
        })
        .collect::<Vec<_>>();

    if fields.is_empty() {
        quote! { struct #name; }
    } else {
        quote! { struct #name( #( #fields ),* ); }
    }
}

/// Expands to the struct definition of a call struct
fn expand_data_struct(name: &Ident, params: &[(TokenStream, TokenStream)]) -> TokenStream {
    let fields = params
        .iter()
        .map(|(name, ty)| {
            quote! { pub #name: #ty }
        })
        .collect::<Vec<_>>();

    quote! { struct #name { #( #fields, )* } }
}

#[cfg(test)]
mod tests {
    use ethers_core::abi::ParamType;

    use super::*;

    // packs the argument in a tuple to be used for the contract call
    fn expand_inputs_call_arg(inputs: &[Param]) -> TokenStream {
        let names = inputs
            .iter()
            .enumerate()
            .map(|(i, param)| {
                let name = util::expand_input_name(i, &param.name);
                match param.kind {
                    // this is awkward edge case where the function inputs are a single struct
                    // we need to force this argument into a tuple so it gets expanded to
                    // `((#name,))` this is currently necessary because
                    // internally `flatten_tokens` is called which removes the outermost `tuple`
                    // level and since `((#name))` is not a rust tuple it
                    // doesn't get wrapped into another tuple that will be peeled off by
                    // `flatten_tokens`
                    ParamType::Tuple(_) if inputs.len() == 1 => {
                        // make sure the tuple gets converted to `Token::Tuple`
                        quote! {(#name,)}
                    }
                    _ => name,
                }
            })
            .collect::<Vec<TokenStream>>();
        match names.len() {
            0 => quote! { () },
            1 => quote! { #( #names )* },
            _ => quote! { ( #(#names, )* ) },
        }
    }

    // converts the function params to name/type pairs
    fn expand_inputs(inputs: &[Param]) -> Result<TokenStream> {
        let params = inputs
            .iter()
            .enumerate()
            .map(|(i, param)| {
                let name = util::expand_input_name(i, &param.name);
                let kind = types::expand(&param.kind)?;
                Ok(quote! { #name: #kind })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(quote! { #( , #params )* })
    }

    #[test]
    fn test_expand_inputs_call_arg() {
        // no inputs
        let params = vec![];
        let token_stream = expand_inputs_call_arg(&params);
        assert_eq!(token_stream.to_string(), "()");

        // single input
        let params = vec![Param {
            name: "arg_a".to_string(),
            kind: ParamType::Address,
            internal_type: None,
        }];
        let token_stream = expand_inputs_call_arg(&params);
        assert_eq!(token_stream.to_string(), "arg_a");

        // two inputs
        let params = vec![
            Param { name: "arg_a".to_string(), kind: ParamType::Address, internal_type: None },
            Param {
                name: "arg_b".to_string(),
                kind: ParamType::Uint(256usize),
                internal_type: None,
            },
        ];
        let token_stream = expand_inputs_call_arg(&params);
        assert_eq!(token_stream.to_string(), "(arg_a , arg_b ,)");

        // three inputs
        let params = vec![
            Param { name: "arg_a".to_string(), kind: ParamType::Address, internal_type: None },
            Param {
                name: "arg_b".to_string(),
                kind: ParamType::Uint(128usize),
                internal_type: None,
            },
            Param { name: "arg_c".to_string(), kind: ParamType::Bool, internal_type: None },
        ];
        let token_stream = expand_inputs_call_arg(&params);
        assert_eq!(token_stream.to_string(), "(arg_a , arg_b , arg_c ,)");
    }

    #[test]
    fn expand_inputs_empty() {
        assert_quote!(expand_inputs(&[]).unwrap().to_string(), {},);
    }

    #[test]
    fn expand_inputs_() {
        assert_quote!(
            expand_inputs(

                &[
                    Param {
                        name: "a".to_string(),
                        kind: ParamType::Bool,
                        internal_type: None,
                    },
                    Param {
                        name: "b".to_string(),
                        kind: ParamType::Address,
                        internal_type: None,
                    },
                ],
            )
            .unwrap(),
            { , a: bool, b: ethers_core::types::Address },
        );
    }

    #[test]
    fn expand_fn_outputs_empty() {
        assert_quote!(expand_fn_outputs(&[],).unwrap(), { () });
    }

    #[test]
    fn expand_fn_outputs_single() {
        assert_quote!(
            expand_fn_outputs(&[Param {
                name: "a".to_string(),
                kind: ParamType::Bool,
                internal_type: None,
            }])
            .unwrap(),
            { bool },
        );
    }

    #[test]
    fn expand_fn_outputs_multiple() {
        assert_quote!(
            expand_fn_outputs(&[
                Param { name: "a".to_string(), kind: ParamType::Bool, internal_type: None },
                Param { name: "b".to_string(), kind: ParamType::Address, internal_type: None },
            ],)
            .unwrap(),
            { (bool, ethers_core::types::Address) },
        );
    }
}
