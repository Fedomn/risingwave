use proc_macro2::TokenStream as TokenStream2;
use proc_macro2::{Ident, Span};
use proc_macro_error::abort;
use syn::{
    self, ext::IdentExt, spanned::Spanned, Field, GenericArgument, Lit, Meta, PathArguments,
    PathSegment, Type,
};

fn extract_type_from_option(option_segment: &PathSegment) -> Type {
    let generic_arg = match &option_segment.arguments {
        PathArguments::AngleBracketed(params) => params.args.first().unwrap(),
        _ => panic!("Option has no angle bracket"),
    };
    match generic_arg {
        GenericArgument::Type(inner_ty) => inner_ty.clone(),
        _ => panic!("Option's argument must be a type"),
    }
}

/// For example:
///   #[prost(enumeration = "data_type::TypeName", tag = "1")]
///   pub type_name: i32,
///
/// Returns "data_type::TypeName".
fn extract_enum_type_from_field(field: &Field) -> Option<Type> {
    use syn::{punctuated::Punctuated, Token};

    // The type must be i32.
    match &field.ty {
        Type::Path(path) => {
            if !path.path.segments.first()?.ident.eq("i32") {
                return None;
            }
        }
        _ => return None,
    };

    let attr = field
        .attrs
        .iter()
        .find(|attr| attr.path.is_ident("prost"))?;

    // `enumeration = "data_type::TypeName", tag = "1"`
    let args = attr
        .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .unwrap();

    let enum_type = args
        .iter()
        .map(|meta| match meta {
            Meta::NameValue(kv) => {
                if kv.path.is_ident("enumeration") {
                    match &kv.lit {
                        // data_type::TypeName
                        Lit::Str(enum_type) => Some(enum_type.to_owned()),
                        _ => None,
                    }
                } else {
                    None
                }
            }
            _ => None,
        })
        .next()??;

    Some(syn::parse_str::<Type>(&enum_type.value()).unwrap())
}

pub fn implement(field: &Field) -> TokenStream2 {
    let field_name = field
        .clone()
        .ident
        .unwrap_or_else(|| abort!(field.span(), "Expected the field to have a name"));

    let getter_fn_name = Ident::new(&format!("get_{}", field_name.unraw()), Span::call_site());

    match extract_enum_type_from_field(field) {
        None => {}
        Some(enum_type) => {
            return quote! {
                #[inline(always)]
                pub fn #getter_fn_name(&self) -> #enum_type {
                    #enum_type::from_i32(self.#field_name).unwrap()
                }
            };
        }
    };

    let ty = field.ty.clone();
    if let Type::Path(ref type_path) = ty {
        let data_type = type_path.path.segments.last().unwrap();
        if data_type.ident == "Option" {
            // ::core::option::Option<Foo>
            let ty = extract_type_from_option(data_type);
            return quote! {
                #[inline(always)]
                pub fn #getter_fn_name(&self) -> &#ty {
                    // TODO: unwrap can panic when option is None
                    &self.#field_name.as_ref().unwrap()
                }
            };
        } else if ["u32", "u64", "f32", "f64", "i32", "i64", "bool"]
            .contains(&data_type.ident.to_string().as_str())
        {
            // Primitive types.
            return quote! {
                #[inline(always)]
                pub fn #getter_fn_name(&self) -> #ty {
                    self.#field_name
                }
            };
        }
    }

    return quote! {
        #[inline(always)]
        pub fn #getter_fn_name(&self) -> &#ty {
            &self.#field_name
        }
    };
}
