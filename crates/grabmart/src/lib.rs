//! GrabMart: the mart API client, models, cart and quote parsing (see
//! docs/mart.md).
//!
//! Browsing uses the mart `public` API with reCAPTCHA tokens minted in the CDP
//! browser ([`recaptcha`]); results are cached under namespace `"mart"`. The
//! cart is a value the caller keeps; quotes hit `protected/cart` with the
//! logged-in session; the order path is gated exactly like GrabFood's
//! ([`order`]). Build a [`client::Mart`] from a [`grabcli_core::Context`].

pub mod api;
pub mod cart;
pub mod client;
pub mod model;
pub mod order;
pub mod quote;
pub mod recaptcha;

pub use cart::{Cart, CartItem};
pub use client::{Mart, MemoryPacer, Pacer};
