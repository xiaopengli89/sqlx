use super::attributes::{
    check_strong_enum_attributes, check_struct_attributes, check_transparent_attributes,
    check_weak_enum_attributes, parse_child_attributes, parse_container_attributes,
};
use super::rename_all;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::{
    parse_quote, Data, DataEnum, DataStruct, DeriveInput, Expr, Field, Fields, FieldsNamed,
    FieldsUnnamed, Stmt, Variant,
};

pub fn expand_derive_encode(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let args = parse_container_attributes(&input.attrs)?;

    match &input.data {
        Data::Struct(DataStruct {
            fields: Fields::Unnamed(FieldsUnnamed { unnamed, .. }),
            ..
        }) if unnamed.len() == 1 => {
            expand_derive_encode_transparent(&input, unnamed.first().unwrap())
        }
        Data::Enum(DataEnum { variants, .. }) => match args.repr {
            Some(_) => expand_derive_encode_weak_enum(input, variants),
            None => expand_derive_encode_strong_enum(input, variants),
        },
        Data::Struct(DataStruct {
            fields: Fields::Named(FieldsNamed { named, .. }),
            ..
        }) => expand_derive_encode_struct(input, named),
        Data::Union(_) => Err(syn::Error::new_spanned(input, "unions are not supported")),
        Data::Struct(DataStruct {
            fields: Fields::Unnamed(..),
            ..
        }) => Err(syn::Error::new_spanned(
            input,
            "structs with zero or more than one unnamed field are not supported",
        )),
        Data::Struct(DataStruct {
            fields: Fields::Unit,
            ..
        }) => Err(syn::Error::new_spanned(
            input,
            "unit structs are not supported",
        )),
    }
}

fn expand_derive_encode_transparent(
    input: &DeriveInput,
    field: &Field,
) -> syn::Result<proc_macro2::TokenStream> {
    check_transparent_attributes(input, field)?;

    let ident = &input.ident;
    let ty = &field.ty;

    // extract type generics
    let generics = &input.generics;
    let (_, ty_generics, _) = generics.split_for_impl();

    // add db type for impl generics & where clause
    let mut generics = generics.clone();
    generics.params.insert(0, parse_quote!(DB: sqlx::Database));
    generics
        .make_where_clause()
        .predicates
        .push(parse_quote!(#ty: sqlx::encode::Encode<DB>));
    let (impl_generics, _, where_clause) = generics.split_for_impl();

    Ok(quote!(
        impl #impl_generics sqlx::encode::Encode<DB> for #ident #ty_generics #where_clause {
            fn encode(&self, buf: &mut DB::RawBuffer) {
                sqlx::encode::Encode::encode(&self.0, buf)
            }
            fn encode_nullable(&self, buf: &mut DB::RawBuffer) -> sqlx::encode::IsNull {
                sqlx::encode::Encode::encode_nullable(&self.0, buf)
            }
            fn size_hint(&self) -> usize {
                sqlx::encode::Encode::size_hint(&self.0)
            }
        }
    ))
}

fn expand_derive_encode_weak_enum(
    input: &DeriveInput,
    variants: &Punctuated<Variant, Comma>,
) -> syn::Result<proc_macro2::TokenStream> {
    let attr = check_weak_enum_attributes(input, &variants)?;
    let repr = attr.repr.unwrap();

    let ident = &input.ident;

    Ok(quote!(
        impl<DB: sqlx::Database> sqlx::encode::Encode<DB> for #ident where #repr: sqlx::encode::Encode<DB> {
            fn encode(&self, buf: &mut DB::RawBuffer) {
                sqlx::encode::Encode::encode(&(*self as #repr), buf)
            }

            fn encode_nullable(&self, buf: &mut DB::RawBuffer) -> sqlx::encode::IsNull {
                sqlx::encode::Encode::encode_nullable(&(*self as #repr), buf)
            }

            fn size_hint(&self) -> usize {
                sqlx::encode::Encode::size_hint(&(*self as #repr))
            }
        }
    ))
}

fn expand_derive_encode_strong_enum(
    input: &DeriveInput,
    variants: &Punctuated<Variant, Comma>,
) -> syn::Result<proc_macro2::TokenStream> {
    let cattr = check_strong_enum_attributes(input, &variants)?;

    let ident = &input.ident;

    let mut value_arms = Vec::new();
    for v in variants {
        let id = &v.ident;
        let attributes = parse_child_attributes(&v.attrs)?;

        if let Some(rename) = attributes.rename {
            value_arms.push(quote!(#ident :: #id => #rename,));
        } else if let Some(pattern) = cattr.rename_all {
            let name = rename_all(&*id.to_string(), pattern);

            value_arms.push(quote!(#ident :: #id => #name,));
        } else {
            let name = id.to_string();
            value_arms.push(quote!(#ident :: #id => #name,));
        }
    }

    Ok(quote!(
        impl<DB: sqlx::Database> sqlx::encode::Encode<DB> for #ident where str: sqlx::encode::Encode<DB> {
            fn encode(&self, buf: &mut DB::RawBuffer) {
                let val = match self {
                    #(#value_arms)*
                };
                <str as sqlx::encode::Encode<DB>>::encode(val, buf)
            }

            fn size_hint(&self) -> usize {
                let val = match self {
                    #(#value_arms)*
                };
                <str as sqlx::encode::Encode<DB>>::size_hint(val)
            }
        }
    ))
}

fn expand_derive_encode_struct(
    input: &DeriveInput,
    fields: &Punctuated<Field, Comma>,
) -> syn::Result<proc_macro2::TokenStream> {
    check_struct_attributes(input, &fields)?;

    let mut tts = proc_macro2::TokenStream::new();

    if cfg!(feature = "postgres") {
        let ident = &input.ident;
        let column_count = fields.len();

        // extract type generics
        let generics = &input.generics;
        let (_, ty_generics, _) = generics.split_for_impl();

        // add db type for impl generics & where clause
        let mut generics = generics.clone();
        let predicates = &mut generics.make_where_clause().predicates;

        for field in fields {
            let ty = &field.ty;

            predicates.push(parse_quote!(#ty: sqlx::encode::Encode<sqlx::Postgres>));
            predicates.push(parse_quote!(#ty: sqlx::types::Type<sqlx::Postgres>));
        }

        let (impl_generics, _, where_clause) = generics.split_for_impl();

        let writes = fields.iter().map(|field| -> Stmt {
            let id = &field.ident;

            parse_quote!(
                // sqlx::postgres::encode_struct_field(buf, &self. #id);
                encoder.encode(&self. #id);
            )
        });

        let sizes = fields.iter().map(|field| -> Expr {
            let id = &field.ident;
            let ty = &field.ty;

            parse_quote!(
                <#ty as sqlx::encode::Encode<sqlx::Postgres>>::size_hint(&self. #id)
            )
        });

        tts.extend(quote!(
            impl #impl_generics sqlx::encode::Encode<sqlx::Postgres> for #ident #ty_generics #where_clause {
                fn encode(&self, buf: &mut sqlx::postgres::PgRawBuffer) {
                    let mut encoder = sqlx::postgres::types::raw::PgRecordEncoder::new(buf);

                    #(#writes)*

                    encoder.finish();
                }

                fn size_hint(&self) -> usize {
                    #column_count * (4 + 4) // oid (int) and length (int) for each column
                        + #(#sizes)+* // sum of the size hints for each column
                }
            }
        ));
    }

    Ok(tts)
}
