// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary's bridge crate.
//!
//! This crate owns the bridge↔daemon **wire-protocol definition**: the
//! MCP (Model Context Protocol) JSON-RPC message types that cross the Unix
//! domain socket between the host-spawned bridge process and the Catenary
//! daemon, their serialization, and a [protocol version constant]
//! ([`PROTOCOL_VERSION`]). Both sides — the bridge process and the daemon —
//! link this one crate, so they structurally cannot disagree about what the
//! protocol *is*, only about which crate version they were built from.
//!
//! The daemon (in the `catenary-cli` package) imports the protocol definition
//! from here. Daemon-side *behavior* — the IPC hello handler, the version
//! comparison, and the mismatch surfacing — stays in the daemon; this crate
//! ships definitions, never daemon logic.
//!
//! # Version seam
//!
//! The crate's own semver ([`version`]) is the comparand a future handshake
//! compares across the wire: it bumps only when the bridge's wire or behavior
//! changes. It is readable at both build sites — the bridge compiles its
//! version in, and the daemon knows the version it links — so a handshake can
//! be built on top of it without either side re-declaring the value.

/// The bridge crate's own semantic version, as compiled into whichever
/// binary links it.
///
/// This is the comparand for the bridge↔daemon version handshake: the crate's
/// semver bumps only when the bridge's wire or behavior changes, so version
/// equality across the wire is protocol sameness. Exposed as an accessor so
/// both build sites (the bridge process and the daemon) read the identical
/// compiled-in value.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The wire-protocol version carried by the bridge↔daemon handshake.
///
/// A monotonically increasing integer that identifies the shape of the
/// [`protocol`] message set. It is a coarser, hand-maintained comparand than
/// [`version`] (which tracks the crate's full semver): bump it when the wire
/// *format* changes in a way both sides must agree on. The handshake itself
/// (comparison and surfacing) lives daemon-side and is out of this crate's
/// scope — this constant is the value that handshake reads.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod protocol;
