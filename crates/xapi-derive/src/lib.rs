use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Data, DeriveInput, Error, Expr, Fields, Lit, Meta, Type};

type Attr = (String, Option<String>);
type Field = (syn::Ident, Type, Vec<Attr>);

#[proc_macro_derive(XapiRoot, attributes(xapi))]
pub fn xapi_root(input: TokenStream) -> TokenStream {
    derive_root(parse_macro_input!(input as DeriveInput))
}
#[proc_macro_derive(XapiRow, attributes(xapi))]
pub fn xapi_row(input: TokenStream) -> TokenStream {
    derive_row(parse_macro_input!(input as DeriveInput))
}

fn derive_root(input: DeriveInput) -> TokenStream {
    let name = input.ident.clone();
    match root_impl(&input, &name) {
        Ok(v) => v.into(),
        Err(e) => e.into_compile_error().into(),
    }
}
fn derive_row(input: DeriveInput) -> TokenStream {
    let name = input.ident.clone();
    match row_impl(&input, &name) {
        Ok(v) => v.into(),
        Err(e) => e.into_compile_error().into(),
    }
}

fn field_data(input: &DeriveInput) -> syn::Result<Vec<Field>> {
    let Data::Struct(data) = &input.data else {
        return Err(Error::new_spanned(input, "Xapi derives require a struct"));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(Error::new_spanned(
            &data.fields,
            "Xapi derives require named fields",
        ));
    };
    named
        .named
        .iter()
        .map(|field| {
            let id = field
                .ident
                .clone()
                .ok_or_else(|| Error::new_spanned(field, "field name is required"))?;
            let attrs = field
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("xapi"))
                .map(parse_attr)
                .collect::<syn::Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
            Ok((id, field.ty.clone(), attrs))
        })
        .collect()
}

fn parse_attr(attr: &syn::Attribute) -> syn::Result<Vec<Attr>> {
    let Meta::List(list) = &attr.meta else {
        return Err(Error::new_spanned(attr, "use #[xapi(...)]"));
    };
    let mut result = Vec::new();
    list.parse_nested_meta(|meta| {
        let key = meta
            .path
            .get_ident()
            .ok_or_else(|| Error::new_spanned(&meta.path, "attribute key must be an identifier"))?
            .to_string();
        let value = if meta.input.peek(syn::Token![=]) {
            let expr: Expr = meta.value()?.parse()?;
            match expr {
                Expr::Lit(v) => match v.lit {
                    Lit::Str(v) => Some(v.value()),
                    Lit::Int(v) => Some(v.base10_digits().to_string()),
                    _ => {
                        return Err(Error::new_spanned(
                            meta.path,
                            "value must be string or integer literal",
                        ))
                    }
                },
                _ => return Err(Error::new_spanned(meta.path, "value must be a literal")),
            }
        } else {
            None
        };
        result.push((key, value));
        Ok(())
    })?;
    if result.is_empty() {
        Err(Error::new_spanned(attr, "empty xapi attribute"))
    } else {
        Ok(result)
    }
}

fn row_impl(input: &DeriveInput, name: &syn::Ident) -> syn::Result<proc_macro2::TokenStream> {
    let fields = field_data(input)?;
    let field_names: Vec<_> = fields.iter().map(|(field, _, _)| field).collect();
    let mut schemas = Vec::new();
    let mut decoders = Vec::new();
    let mut owned_decoders = Vec::new();
    let mut encoders = Vec::new();
    let mut borrowed_slots = Vec::new();
    let mut owned_slots = Vec::new();
    let mut borrowed_arms = Vec::new();
    let mut owned_arms = Vec::new();
    let mut const_validators = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (field, ty, attrs) in &fields {
        let mut id = None;
        let mut const_value = None;
        let mut is_const = false;
        let mut wire = None;
        let mut size: Option<usize> = None;
        for (key, value) in attrs {
            match key.as_str() {
                "col" => {
                    if id.is_some() {
                        return Err(Error::new_spanned(ty, "conflicting column attributes"));
                    }
                    id = value.clone();
                }
                "const_col" => {
                    if id.is_some() {
                        return Err(Error::new_spanned(ty, "conflicting column attributes"));
                    }
                    id = value.clone();
                    is_const = true;
                }
                "wire" => wire = value.clone(),
                "value" => const_value = value.clone(),
                "size" => {
                    let Some(value) = value else {
                        return Err(Error::new_spanned(ty, "size requires a positive integer"));
                    };
                    let parsed = value
                        .parse::<usize>()
                        .map_err(|_| Error::new_spanned(ty, "size requires a positive integer"))?;
                    if parsed == 0 {
                        return Err(Error::new_spanned(ty, "size requires a positive integer"));
                    }
                    size = Some(parsed);
                }
                "row_type" | "org_row" => {
                    return Err(Error::new_spanned(
                        ty,
                        "row_type/org_row fields are represented by Vec<Tracked<Row>> on the root dataset",
                    ));
                }
                _ => {
                    return Err(Error::new_spanned(
                        ty,
                        format!("unsupported row attribute: {key}"),
                    ))
                }
            }
        }
        let id = id.ok_or_else(|| Error::new_spanned(ty, "row fields require col or const_col"))?;
        if is_const && const_value.is_none() {
            return Err(Error::new_spanned(ty, "const_col requires a literal value"));
        }
        if !is_const && const_value.is_some() {
            return Err(Error::new_spanned(ty, "value is only valid with const_col"));
        }
        if id.is_empty() || !seen.insert(id.clone()) {
            return Err(Error::new_spanned(ty, "duplicate or empty column id"));
        }
        let wire_name = wire.ok_or_else(|| Error::new_spanned(ty, "row fields require wire"))?;
        if (wire_name.eq_ignore_ascii_case("DECIMAL")
            || wire_name.eq_ignore_ascii_case("BIGDECIMAL"))
            && type_is_float(ty)
        {
            return Err(Error::new_spanned(
                ty,
                "DECIMAL/BIGDECIMAL cannot be mapped to f32/f64 without an explicit precision-loss opt-in",
            ));
        }
        if wire_name.eq_ignore_ascii_case("BLOB") != type_is_blob(ty) {
            return Err(Error::new_spanned(
                ty,
                "BLOB columns require Blob or Vec<u8>, and those types require wire = BLOB",
            ));
        }
        let wire = wire_tokens(&wire_name, ty)?;
        let slot = format_ident!("__xapi_slot_{}", field);
        borrowed_slots.push(quote!(let mut #slot: Option<&::xapi::RawValue> = None;));
        owned_slots.push(quote!(let mut #slot: Option<::xapi::RawValue> = None;));
        borrowed_arms.push(quote! {
            #id => {
                if #slot.is_some() {
                    return Err(::xapi::Error::from(::xapi::DecodeError::message(
                        concat!("duplicate column: ", #id),
                    )));
                }
                #slot = Some(value);
            }
        });
        owned_arms.push(quote! {
            #id => {
                if #slot.is_some() {
                    return Err(::xapi::Error::from(::xapi::DecodeError::message(
                        concat!("duplicate column: ", #id),
                    )));
                }
                #slot = Some(value);
            }
        });
        let size = match size {
            Some(value) => quote!(Some(#value)),
            None => quote!(None),
        };
        let const_schema = match const_value.as_ref() {
            Some(value) => quote!(Some(#value)),
            None => quote!(None),
        };
        let is_const = const_value.is_some();
        schemas.push(quote!(::xapi::FieldSchema { id: #id, wire: #wire, size: #size, rust_type: stringify!(#ty), const_value: #const_schema }));
        if let Some(value) = const_value {
            const_validators.push(quote! {
                #id => {
                    let expected_value: #ty = <#ty as ::xapi::XapiField>::decode_field_with_limits(
                        &::xapi::RawValue::Text(#value.to_owned()), #wire, limits,
                    ).map_err(::xapi::Error::from)?;
                    let actual_value: #ty = <#ty as ::xapi::XapiField>::decode_field_with_limits(
                        value, #wire, limits,
                    ).map_err(::xapi::Error::from)?;
                    let expected = <#ty as ::xapi::XapiField>::encode_field(&expected_value, #wire)
                        .map_err(::xapi::Error::from)?;
                    let actual = <#ty as ::xapi::XapiField>::encode_field(&actual_value, #wire)
                        .map_err(::xapi::Error::from)?;
                    if actual != expected {
                        return Err(::xapi::Error::from(::xapi::DecodeError::message(
                            concat!("constant column mismatch: ", #id),
                        )));
                    }
                }
            });
            decoders.push(quote! {
                let #field: #ty = <#ty as ::xapi::XapiField>::decode_field_with_limits(#slot.as_deref().unwrap_or(&::xapi::RawValue::Text(#value.to_owned())), #wire, limits).map_err(|source| ::xapi::Error::Decode { path: ::xapi::Path::new(concat!(stringify!(#name), ".", #id)), source })?;
                let expected_value: #ty = <#ty as ::xapi::XapiField>::decode_field_with_limits(&::xapi::RawValue::Text(#value.to_owned()), #wire, limits).map_err(::xapi::Error::from)?;
                let expected = <#ty as ::xapi::XapiField>::encode_field(&expected_value, #wire).map_err(::xapi::Error::from)?;
                let actual = <#ty as ::xapi::XapiField>::encode_field(&#field, #wire).map_err(::xapi::Error::from)?;
                if actual != expected {
                    return Err(::xapi::Error::from(::xapi::DecodeError::message(concat!("constant column mismatch: ", #id))));
                }
            });
            owned_decoders.push(quote! {
                let #field: #ty = <#ty as ::xapi::XapiField>::decode_field_owned(#slot.take().unwrap_or(::xapi::RawValue::Text(#value.to_owned())), #wire, limits).map_err(|source| ::xapi::Error::Decode { path: ::xapi::Path::new(concat!(stringify!(#name), ".", #id)), source })?;
                let expected_value: #ty = <#ty as ::xapi::XapiField>::decode_field_with_limits(&::xapi::RawValue::Text(#value.to_owned()), #wire, limits).map_err(::xapi::Error::from)?;
                let expected = <#ty as ::xapi::XapiField>::encode_field(&expected_value, #wire).map_err(::xapi::Error::from)?;
                let actual = <#ty as ::xapi::XapiField>::encode_field(&#field, #wire).map_err(::xapi::Error::from)?;
                if actual != expected {
                    return Err(::xapi::Error::from(::xapi::DecodeError::message(concat!("constant column mismatch: ", #id))));
                }
            });
        } else {
            decoders.push(quote!(let #field: #ty = <#ty as ::xapi::XapiField>::decode_field_with_limits(#slot.as_deref().unwrap_or(&::xapi::RawValue::Missing), #wire, limits).map_err(|source| ::xapi::Error::Decode { path: ::xapi::Path::new(concat!(stringify!(#name), ".", #id)), source })?;));
            owned_decoders.push(quote!(let #field: #ty = <#ty as ::xapi::XapiField>::decode_field_owned(#slot.take().unwrap_or(::xapi::RawValue::Missing), #wire, limits).map_err(|source| ::xapi::Error::Decode { path: ::xapi::Path::new(concat!(stringify!(#name), ".", #id)), source })?;));
        }
        if !is_const {
            encoders.push(quote!((#id.to_owned(), <#ty as ::xapi::XapiField>::encode_field(&self.#field, #wire)?)));
        }
    }
    let borrowed_dispatch = quote! {
        #(#borrowed_slots)*
        for (id, value) in row {
            match id.as_str() {
                #(#borrowed_arms,)*
                _ => {
                    if strict {
                        return Err(::xapi::Error::from(::xapi::DecodeError::message(
                            format!("unknown column: {id}"),
                        )));
                    }
                }
            }
        }
    };
    let owned_dispatch = quote! {
        #(#owned_slots)*
        for (id, value) in row {
            match id.as_str() {
                #(#owned_arms,)*
                _ => {
                    if strict {
                        return Err(::xapi::Error::from(::xapi::DecodeError::message(
                            format!("unknown column: {id}"),
                        )));
                    }
                }
            }
        }
    };
    let const_validation = if const_validators.is_empty() {
        quote! {
            Err(::xapi::Error::from(::xapi::DecodeError::message(format!(
                "unknown constant column: {id}"
            ))))
        }
    } else {
        quote! {
            match id {
                #(#const_validators,)*
                _ => return Err(::xapi::Error::from(::xapi::DecodeError::message(format!("unknown constant column: {id}")))),
            }
            Ok(())
        }
    };
    Ok(quote! { impl ::xapi::XapiRow for #name {
        fn schema() -> &'static ::xapi::RowSchema<Self> { static FIELDS: &[::xapi::FieldSchema] = &[#(#schemas),*]; static SCHEMA: ::xapi::RowSchema<#name> = ::xapi::RowSchema { fields: FIELDS, marker: ::std::marker::PhantomData }; &SCHEMA }
        fn decode_raw(row: &[(String, ::xapi::RawValue)]) -> ::xapi::Result<Self> { Self::decode_raw_with_options(row, false, ::xapi::Limits::default()) }
        fn decode_raw_with_limits(row: &[(String, ::xapi::RawValue)], limits: ::xapi::Limits) -> ::xapi::Result<Self> { Self::decode_raw_with_options(row, false, limits) }
        fn decode_raw_with_options(row: &[(String, ::xapi::RawValue)], strict: bool, limits: ::xapi::Limits) -> ::xapi::Result<Self> {
            #borrowed_dispatch
            #(#decoders)* Ok(Self { #(#field_names),* })
        }
        fn decode_raw_owned(row: Vec<(String, ::xapi::RawValue)>, strict: bool, limits: ::xapi::Limits) -> ::xapi::Result<Self> {
            #owned_dispatch
            #(#owned_decoders)* Ok(Self { #(#field_names),* })
        }
        fn encode_raw(&self) -> ::xapi::Result<Vec<(String, ::xapi::RawValue)>> { Ok(vec![#(#encoders),*]) }
        fn validate_const_column(id: &str, value: &::xapi::RawValue, limits: ::xapi::Limits) -> ::xapi::Result<()> {
            #const_validation
        }
    } })
}

fn root_impl(input: &DeriveInput, name: &syn::Ident) -> syn::Result<proc_macro2::TokenStream> {
    let fields = field_data(input)?;
    let decoder_name = format_ident!("{}Decoder", name);
    let field_names: Vec<_> = fields.iter().map(|(field, _, _)| field).collect();
    let mut schemas = Vec::new();
    let mut decoders = Vec::new();
    let mut encoders = Vec::new();
    let mut decoder_fields = Vec::new();
    let mut decoder_init = Vec::new();
    let mut parameter_arms = Vec::new();
    let mut dataset_begin_arms = Vec::new();
    let mut dataset_arms = Vec::new();
    let mut decoder_finish = Vec::new();
    let mut strict_dataset_checks = Vec::new();
    let mut stream_parameters = Vec::new();
    let mut stream_datasets = Vec::new();
    let mut dataset_wire_arms = Vec::new();
    let mut dataset_blob_arms = Vec::new();
    let mut dataset_const_arms = Vec::new();
    decoder_fields.push(quote!(pub strict: bool));
    decoder_fields.push(quote!(pub limits: ::xapi::Limits));
    decoder_init.push(quote!(strict));
    decoder_init.push(quote!(limits));
    let mut seen = std::collections::HashSet::new();
    for (field, ty, attrs) in &fields {
        let mappings: Vec<_> = attrs
            .iter()
            .filter(|(key, _)| key == "param" || key == "dataset")
            .collect();
        if mappings.len() != 1
            || attrs
                .iter()
                .any(|(key, _)| key != "param" && key != "dataset" && key != "wire")
        {
            return Err(Error::new_spanned(
                ty,
                "root fields require param or dataset, with optional wire",
            ));
        }
        let (key, id) = mappings[0];
        let wire_name = attrs
            .iter()
            .find(|(attr, _)| attr == "wire")
            .and_then(|(_, value)| value.clone());
        let wire = match wire_name {
            Some(value) => wire_tokens(&value, ty)?,
            None if key == "dataset" => quote!(::xapi::WireType::String),
            None => return Err(Error::new_spanned(ty, "parameter fields require wire")),
        };
        let kind = match key.as_str() {
            "param" => quote!(::xapi::RootFieldKind::Parameter),
            "dataset" => quote!(::xapi::RootFieldKind::Dataset),
            _ => {
                return Err(Error::new_spanned(
                    ty,
                    "root fields require param or dataset",
                ))
            }
        };
        let id = id
            .clone()
            .ok_or_else(|| Error::new_spanned(ty, "root mapping requires an id"))?;
        if id.is_empty() || !seen.insert(id.clone()) {
            return Err(Error::new_spanned(ty, "duplicate or empty root id"));
        }
        schemas.push(quote!(::xapi::RootFieldSchema { id: #id, kind: #kind, wire: #wire, rust_type: stringify!(#ty) }));
        if key == "param" {
            decoder_fields.push(quote!(pub #field: Option<#ty>));
            decoder_init.push(quote!(#field: None));
            decoder_finish.push(quote!(#field: self.#field.ok_or_else(|| ::xapi::Error::from(::xapi::DecodeError::message(concat!("missing parameter: ", #id))))?));
            parameter_arms.push(quote!(#id => { self.#field = Some(<#ty as ::xapi::XapiField>::decode_field_with_limits(value, #wire, self.limits).map_err(|source| ::xapi::Error::Decode { path: ::xapi::Path::new(concat!(stringify!(#name), ".", #id)), source })?); }));
            decoders.push(quote!(let #field: #ty = <#ty as ::xapi::XapiField>::decode_field(::xapi::raw_get(&document.parameters, #id), #wire).map_err(|source| ::xapi::Error::Decode { path: ::xapi::Path::new(concat!(stringify!(#name), ".", #id)), source })?;));
            encoders.push(quote!(parameters.push((#id.to_owned(), <#ty as ::xapi::XapiField>::encode_field(&self.#field, #wire)?));));
            stream_parameters.push(quote!(sink.parameter(#id, <#ty as ::xapi::XapiField>::encode_field(&self.#field, #wire)?, #wire)?;));
        } else {
            let element_ty = vec_inner(ty)?;
            let seen_field = format_ident!("__xapi_seen_{}", field);
            decoder_fields.push(quote!(#seen_field: bool));
            decoder_init.push(quote!(#seen_field: false));
            dataset_begin_arms.push(quote!(#id => { self.#seen_field = true; }));
            strict_dataset_checks.push(quote! {
                if self.strict && !self.#seen_field {
                    return Err(::xapi::Error::from(::xapi::DecodeError::message(concat!("missing dataset: ", #id))));
                }
            });
            if let Some(row_ty) = tracked_inner(&element_ty) {
                dataset_wire_arms.push(quote!(#id => <#row_ty as ::xapi::XapiRow>::schema().fields.iter().find(|field| field.id == column).map(|field| field.wire)));
                dataset_blob_arms.push(quote!(#id => <#row_ty as ::xapi::XapiRow>::schema().fields.iter().any(|field| field.wire == ::xapi::WireType::Blob)));
                dataset_const_arms.push(quote!(#id => {
                    match <#row_ty as ::xapi::XapiRow>::schema().fields.iter().find(|field| field.id == id) {
                        Some(field) if field.const_value.is_some() => <#row_ty as ::xapi::XapiRow>::validate_const_column(id, value, self.limits),
                        Some(_) | None if self.strict => Err(::xapi::Error::from(::xapi::DecodeError::message(format!("unknown constant column: {id}")))),
                        Some(_) | None => Ok(()),
                    }
                }));
                decoder_fields.push(quote!(pub #field: #ty));
                decoder_init.push(quote!(#field: Vec::new()));
                decoder_finish.push(quote!(#field: self.#field));
                dataset_arms.push(quote!(#id => {
                    let row_index = self.#field.len();
                    let row_type = row.row_type;
                    let current = <#row_ty as ::xapi::XapiRow>::decode_raw_owned(row.values, self.strict, self.limits)
                        .map_err(|error| error.prepend_path(format!(concat!(stringify!(#name), ".", #id, "[{}]"), row_index)))?;
                    let original = match row.original {
                        Some(original) => Some(Box::new(<#row_ty as ::xapi::XapiRow>::decode_raw_owned(original, self.strict, self.limits)
                            .map_err(|error| error.prepend_path(format!(concat!(stringify!(#name), ".", #id, "[{}].original"), row_index)))?)),
                        None => None,
                    };
                    self.#field.push(::xapi::Tracked { row_type, current, original });
                }));
                decoders.push(quote!(let #field: #ty = match document.datasets.iter().find(|dataset| dataset.id == #id) { Some(dataset) => dataset.rows.iter().map(|raw| { let current = <#row_ty as ::xapi::XapiRow>::decode_raw(&raw.values)?; let original = match raw.original.as_ref() { Some(row) => Some(Box::new(<#row_ty as ::xapi::XapiRow>::decode_raw(row)?)), None => None }; Ok(::xapi::Tracked { row_type: raw.row_type, current, original }) }).collect::<::xapi::Result<Vec<_>>>()?, None => Vec::new() };));
                encoders.push(quote!(datasets.push(::xapi::RawDataset { id: #id.to_owned(), columns: <#row_ty as ::xapi::XapiRow>::schema().fields.iter().filter(|field| field.const_value.is_none()).map(|field| (field.id.to_owned(), field.wire)).collect(), const_columns: <#row_ty as ::xapi::XapiRow>::schema().fields.iter().filter_map(|field| field.const_value.map(|value| (field.id.to_owned(), field.wire, ::xapi::RawValue::Text(value.to_owned())))).collect(), rows: self.#field.iter().map(|tracked| Ok(::xapi::RawRow { row_type: tracked.row_type, values: <#row_ty as ::xapi::XapiRow>::encode_raw(&tracked.current)?, original: match tracked.original.as_ref() { Some(original) => Some(<#row_ty as ::xapi::XapiRow>::encode_raw(original)?), None => None } })).collect::<::xapi::Result<Vec<_>>>()? });));
                stream_datasets.push(quote!(sink.dataset_begin(#id, <#row_ty as ::xapi::XapiRow>::schema().fields)?; for tracked in &self.#field { sink.row(::xapi::RawRow { row_type: tracked.row_type, values: <#row_ty as ::xapi::XapiRow>::encode_raw(&tracked.current)?, original: match tracked.original.as_ref() { Some(original) => Some(<#row_ty as ::xapi::XapiRow>::encode_raw(original)?), None => None } })?; } sink.dataset_end()?;));
            } else {
                let row_ty = element_ty;
                dataset_wire_arms.push(quote!(#id => <#row_ty as ::xapi::XapiRow>::schema().fields.iter().find(|field| field.id == column).map(|field| field.wire)));
                dataset_blob_arms.push(quote!(#id => <#row_ty as ::xapi::XapiRow>::schema().fields.iter().any(|field| field.wire == ::xapi::WireType::Blob)));
                dataset_const_arms.push(quote!(#id => {
                    match <#row_ty as ::xapi::XapiRow>::schema().fields.iter().find(|field| field.id == id) {
                        Some(field) if field.const_value.is_some() => <#row_ty as ::xapi::XapiRow>::validate_const_column(id, value, self.limits),
                        Some(_) | None if self.strict => Err(::xapi::Error::from(::xapi::DecodeError::message(format!("unknown constant column: {id}")))),
                        Some(_) | None => Ok(()),
                    }
                }));
                decoder_fields.push(quote!(pub #field: #ty));
                decoder_init.push(quote!(#field: Vec::new()));
                decoder_finish.push(quote!(#field: self.#field));
                dataset_arms.push(quote!(#id => {
                    let row_index = self.#field.len();
                    let row = <#row_ty as ::xapi::XapiRow>::decode_raw_owned(row.values, self.strict, self.limits)
                        .map_err(|error| error.prepend_path(format!(concat!(stringify!(#name), ".", #id, "[{}]"), row_index)))?;
                    self.#field.push(row);
                }));
                decoders.push(quote!(let #field: #ty = match document.datasets.iter().find(|dataset| dataset.id == #id) { Some(dataset) => dataset.rows.iter().map(|raw| <#row_ty as ::xapi::XapiRow>::decode_raw(&raw.values)).collect::<::xapi::Result<Vec<_>>>()?, None => Vec::new() };));
                encoders.push(quote!(datasets.push(::xapi::RawDataset { id: #id.to_owned(), columns: <#row_ty as ::xapi::XapiRow>::schema().fields.iter().filter(|field| field.const_value.is_none()).map(|field| (field.id.to_owned(), field.wire)).collect(), const_columns: <#row_ty as ::xapi::XapiRow>::schema().fields.iter().filter_map(|field| field.const_value.map(|value| (field.id.to_owned(), field.wire, ::xapi::RawValue::Text(value.to_owned())))).collect(), rows: self.#field.iter().map(|row| Ok(::xapi::RawRow { row_type: ::xapi::RowType::Normal, values: <#row_ty as ::xapi::XapiRow>::encode_raw(row)?, original: None })).collect::<::xapi::Result<Vec<_>>>()? });));
                stream_datasets.push(quote!(sink.dataset_begin(#id, <#row_ty as ::xapi::XapiRow>::schema().fields)?; for row in &self.#field { sink.row(::xapi::RawRow { row_type: ::xapi::RowType::Normal, values: <#row_ty as ::xapi::XapiRow>::encode_raw(row)?, original: None })?; } sink.dataset_end()?;));
            }
        }
    }
    Ok(quote! {
        pub struct #decoder_name { #(#decoder_fields),* }
        impl ::xapi::XapiRootDecoder<#name> for #decoder_name {
            fn unknown_root_member(&mut self, id: &str) -> ::xapi::Result<()> {
                if self.strict {
                    return Err(::xapi::Error::from(::xapi::DecodeError::message(format!("unknown root member: {id}"))));
                }
                Ok(())
            }
            fn parameter(&mut self, id: &str, value: &::xapi::RawValue) -> ::xapi::Result<()> { match id { #(#parameter_arms,)* _ => { if self.strict { return Err(::xapi::Error::from(::xapi::DecodeError::message(format!("unknown parameter: {id}")))); } } } Ok(()) }
            fn dataset_begin(&mut self, dataset: &str) -> ::xapi::Result<()> { match dataset { #(#dataset_begin_arms,)* _ => { if self.strict { return Err(::xapi::Error::from(::xapi::DecodeError::message(format!("unknown dataset: {dataset}")))); } } } Ok(()) }
            fn dataset_columns(&mut self, dataset: &str, columns: &[(String, ::xapi::WireType)]) -> ::xapi::Result<()> {
                if self.strict {
                    for (id, _) in columns {
                        if <#name as ::xapi::XapiRoot>::dataset_wire(dataset, id).is_none() {
                            return Err(::xapi::Error::from(::xapi::DecodeError::message(format!("unknown column: {id}"))));
                        }
                    }
                }
                Ok(())
            }
            fn dataset_const_column(&mut self, dataset: &str, id: &str, _wire: ::xapi::WireType, value: &::xapi::RawValue) -> ::xapi::Result<()> {
                match dataset {
                    #(#dataset_const_arms,)*
                    _ => {
                        if self.strict {
                            return Err(::xapi::Error::from(::xapi::DecodeError::message(format!("unknown dataset: {dataset}"))));
                        }
                        Ok(())
                    }
                }
            }
            fn dataset_row(&mut self, dataset: &str, row: ::xapi::RawRow) -> ::xapi::Result<()> { match dataset { #(#dataset_arms,)* _ => { if self.strict { return Err(::xapi::Error::from(::xapi::DecodeError::message(format!("unknown dataset: {dataset}")))); } } } Ok(()) }
            fn finish(self) -> ::xapi::Result<#name> { #(#strict_dataset_checks)* Ok(#name { #(#decoder_finish),* }) }
        }
        impl ::xapi::XapiRoot for #name {
        type Decoder = #decoder_name;
        fn decoder(strict: bool, limits: ::xapi::Limits) -> Self::Decoder { #decoder_name { #(#decoder_init),* } }
        fn dataset_wire(dataset: &str, column: &str) -> Option<::xapi::WireType> { match dataset { #(#dataset_wire_arms,)* _ => None } }
        fn dataset_has_blob(dataset: &str) -> bool { match dataset { #(#dataset_blob_arms,)* _ => false } }
        fn schema() -> &'static ::xapi::RootSchema<Self> { static FIELDS: &[::xapi::RootFieldSchema] = &[#(#schemas),*]; static SCHEMA: ::xapi::RootSchema<#name> = ::xapi::RootSchema { fields: FIELDS, marker: ::std::marker::PhantomData }; &SCHEMA }
        fn decode_raw(document: ::xapi::RawDocument) -> ::xapi::Result<Self> { #(#decoders)* Ok(Self { #(#field_names),* }) }
        fn encode_raw(&self) -> ::xapi::Result<::xapi::RawDocument> { let mut parameters = Vec::new(); let mut datasets = Vec::new(); #(#encoders)* Ok(::xapi::RawDocument { parameters, datasets }) }
        fn encode_stream(&self, sink: &mut dyn ::xapi::XapiEncodeSink) -> ::xapi::Result<()> { sink.begin()?; #(#stream_parameters)* #(#stream_datasets)* sink.end() }
    } })
}

fn vec_inner(ty: &Type) -> syn::Result<Type> {
    let Type::Path(path) = ty else {
        return Err(Error::new_spanned(ty, "dataset field must be Vec<Row>"));
    };
    let segment = path
        .path
        .segments
        .last()
        .ok_or_else(|| Error::new_spanned(ty, "dataset field must be Vec<Row>"))?;
    if segment.ident != "Vec" {
        return Err(Error::new_spanned(ty, "dataset field must be Vec<Row>"));
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(Error::new_spanned(ty, "dataset field must be Vec<Row>"));
    };
    match args.args.first() {
        Some(syn::GenericArgument::Type(inner)) => Ok(inner.clone()),
        _ => Err(Error::new_spanned(ty, "dataset field must be Vec<Row>")),
    }
}

fn tracked_inner(ty: &Type) -> Option<Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "Tracked" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    match args.args.first() {
        Some(syn::GenericArgument::Type(inner)) => Some(inner.clone()),
        _ => None,
    }
}

fn type_is_float(ty: &Type) -> bool {
    let ty = unwrap_option(ty);
    match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident == "f32" || segment.ident == "f64")
            .unwrap_or(false),
        _ => false,
    }
}

fn type_is_blob(ty: &Type) -> bool {
    let ty = unwrap_option(ty);
    let Type::Path(path) = ty else {
        return false;
    };
    let Some(segment) = path.path.segments.last() else {
        return false;
    };
    if segment.ident == "Blob" {
        return true;
    }
    if segment.ident != "Vec" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return false;
    };
    matches!(args.args.first(), Some(syn::GenericArgument::Type(Type::Path(path)))
        if path.path.segments.last().is_some_and(|segment| segment.ident == "u8"))
}

fn unwrap_option(mut ty: &Type) -> &Type {
    loop {
        let Type::Path(path) = ty else {
            return ty;
        };
        let Some(segment) = path.path.segments.last() else {
            return ty;
        };
        if segment.ident != "Option" {
            return ty;
        }
        let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
            return ty;
        };
        let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
            return ty;
        };
        ty = inner;
    }
}

fn wire_tokens(wire: &str, span: &Type) -> syn::Result<proc_macro2::TokenStream> {
    let name = match wire.to_ascii_uppercase().as_str() {
        "STRING" => "String",
        "INT" => "Int",
        "LONG" => "Long",
        "FLOAT" => "Float",
        "DOUBLE" => "Double",
        "DECIMAL" => "Decimal",
        "BIGDECIMAL" => "BigDecimal",
        "DATE" => "Date",
        "TIME" => "Time",
        "DATETIME" => "DateTime",
        "BLOB" => "Blob",
        "BOOLEAN" => "Boolean",
        _ => return Err(Error::new_spanned(span, "unsupported wire type")),
    };
    let ident = syn::Ident::new(name, proc_macro2::Span::call_site());
    Ok(quote!(::xapi::WireType::#ident))
}
