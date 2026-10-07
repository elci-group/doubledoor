//! Distributed Doubledoor (§51 Phase VII).
//!
//! A *remote* is another Doubledoor serving a clone of the same canonical
//! repository — on another host (reached through a transport such as
//! `ssh -oBatchMode=yes host`) or on this one. Doubledoor talks to it with
//! the Phase VI protocol (`doubledoor.protocol/v1`) over the transport's
//! stdin/stdout: the protocol is the wire, the transport is just a pipe.
//!
//! Remote command lines consist only of fixed tokens and validated
//! configuration values ([`config::safe_token`]); everything a request
//! carries travels as JSON on stdin. This matters because SSH hands its
//! command line to a remote shell (§27: no shell concatenation of
//! untrusted values).

#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod node;

pub use client::{Client, RemoteError};
pub use config::{Remote, RemotesConfig};
