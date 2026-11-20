//! The cases, one module per subject. Each case is a `pub async fn` taking
//! a [`Fixture`](super::Fixture), and is listed in
//! [`for_each_case!`](crate::for_each_case), which is what runs it.

pub mod fixture;
