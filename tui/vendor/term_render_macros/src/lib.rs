use proc_macro::TokenStream;
use quote::quote;

use syn::{parse_macro_input, Expr, Token};
use syn::punctuated::Punctuated;

/// Expands to a call to the Colorize trait, reducing boilerplate. If a single value is provided, it
/// converts to a call to `Colorize::colorize` with the provided value. If none,
/// or multiple values are provided, it converts to a call to `Colorize::colorizes`
/// with a vector of the provided values.
/// This macro can be used with any type that implements the Colorize trait, including:
/// * `Colored`
/// * `String`
/// * `str`
/// # Parameters
/// - value of type T with trait Colorize
/// - n ColorType variants where n is [0, ∞)
/// # Example
/// ```
/// term_render::color!("Hello World", White, Bold, Underline);
/// term_render::color!("Hello World");  // converts to Colored without applying modifiers
/// ```
#[proc_macro]
pub fn color (input: TokenStream) -> TokenStream {
    let args: Punctuated<Expr, Token![,]> = parse_macro_input!(input with Punctuated::parse_terminated);
    let string = &args[0];
    let variants: Vec <_> = args.iter().skip(1).map(|ident| quote! { ColorType::#ident }).collect();

    if variants.len() == 1 {
        let arg = &args[1];
        return TokenStream::from (quote! {
            #string.colorize(ColorType::#arg)
        })
    }
    TokenStream::from (quote! {
        #string.colorizes(vec![#(#variants),*])
    })
}

/// Expands the provided value into an `Arc<RwLock<T>>`.
/// This is a shorthand macro to avoid writing the full expression every time (i.e.
/// `std::sync::Arc::new(parking_lot::RwLock::new(value))` ).
/// # Parameters
/// - value of type T
/// # Example
/// ```
/// let data = send_sync!(MyStruct { field1: 10, field2: String::from("Hello") });
/// // instead of
/// let data = std::sync::Arc::new(parking_lot::RwLock::new(MyStruct { field1: 10, field2: String::from("Hello") }));
/// ```
#[proc_macro]
pub fn send_sync (input: TokenStream) -> TokenStream {
    let args: Punctuated<Expr, Token![,]> = parse_macro_input!(input with Punctuated::parse_terminated);
    let value = &args[0];
    TokenStream::from (quote! {
        std::sync::Arc::new(parking_lot::RwLock::new(#value))
    })
}

