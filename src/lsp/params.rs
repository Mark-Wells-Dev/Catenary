// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Builder functions for LSP request and notification parameters.
//!
//! One function per LSP method Catenary uses. Each constructs a
//! `serde_json::Value` from primitives — no `lsp_types` dependency.

use serde_json::{Value, json};

// ── Private helpers ─────────────────────────────────────────────────

/// Builds `TextDocumentPositionParams` — shared by definition,
/// type definition, implementation, prepare rename, and hierarchy prepares.
fn text_document_position(uri: &str, line: u32, character: u32) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    })
}

/// Converts `(uri, name)` pairs to a JSON array of workspace folders.
fn folder_array(folders: &[(&str, &str)]) -> Vec<Value> {
    folders
        .iter()
        .map(|(uri, name)| json!({ "uri": uri, "name": name }))
        .collect()
}

// ── Lifecycle ───────────────────────────────────────────────────────

/// Builds `InitializeParams` with the `ClientCapabilities` Catenary advertises
/// to the named server, shaped by its conformance profile (misc 157).
///
/// The server's [`super::server_behavior::ServerProfile`] is resolved once and
/// drives both conformance seams: [a pull-suppressed
/// server](super::server_behavior::ServerProfile::shape_client_capabilities) does
/// **not** receive the `textDocument.diagnostic` client capability (every other
/// server receives today's shape unchanged), and forced
/// [`initializationOptions`](super::server_behavior::ServerProfile::effective_initialization_options)
/// are overlaid onto — and win over — any user-supplied options.
///
/// `roots` is a slice of `(uri, name)` pairs for workspace folders.
#[must_use]
pub fn initialize(
    pid: u32,
    roots: &[(&str, &str)],
    server_name: &str,
    initialization_options: Option<&Value>,
) -> Value {
    // Per the LSP spec: null workspaceFolders means "single file open,
    // no workspace." An empty array means "workspace open, no folders
    // configured." Use null for single-file mode.
    let workspace_folders: Value = if roots.is_empty() {
        Value::Null
    } else {
        json!(folder_array(roots))
    };
    let root_uri = roots.first().map_or(Value::Null, |(uri, _)| json!(uri));

    let mut params = json!({
        "processId": pid,
        "rootUri": root_uri,
        "capabilities": {
            "general": {
                "positionEncodings": ["utf-8", "utf-16"]
            },
            "textDocument": {
                "synchronization": {
                    "didSave": true,
                    "dynamicRegistration": false,
                    "willSave": false,
                    "willSaveWaitUntil": false
                },
                "publishDiagnostics": {
                    "versionSupport": true
                },
                "definition": {
                    "dynamicRegistration": false,
                    "linkSupport": true
                },
                "typeDefinition": {
                    "dynamicRegistration": false,
                    "linkSupport": true
                },
                "implementation": {
                    "dynamicRegistration": false,
                    "linkSupport": true
                },
                "declaration": {
                    "dynamicRegistration": false,
                    "linkSupport": true
                },
                "references": {
                    "dynamicRegistration": false
                },
                "documentSymbol": {
                    "dynamicRegistration": false,
                    "hierarchicalDocumentSymbolSupport": true
                },
                "callHierarchy": {
                    "dynamicRegistration": false
                },
                "typeHierarchy": {
                    "dynamicRegistration": false
                },
                "codeAction": {
                    "dynamicRegistration": false
                },
                "diagnostic": {
                    "dynamicRegistration": false
                }
            },
            "workspace": {
                "symbol": {
                    "resolveSupport": {
                        "properties": ["location.range"]
                    }
                },
                "workspaceFolders": false,
                "configuration": true,
                "didChangeConfiguration": {
                    "dynamicRegistration": true
                },
                "didChangeWatchedFiles": {
                    "dynamicRegistration": true,
                    "relativePatternSupport": true
                }
            },
            "window": {
                "workDoneProgress": true
            }
        },
        "workspaceFolders": workspace_folders
    });

    // Resolve the server's conformance profile once; both seams below consult it,
    // so this builder never tests a server name itself (misc 157).
    let profile = super::server_behavior::ServerProfile::for_server(server_name);

    // Client-capability shaping: a pull-suppressed server loses the
    // `textDocument.diagnostic` capability; an un-profiled server is untouched.
    if let Some(caps) = params.get_mut("capabilities") {
        profile.shape_client_capabilities(caps);
    }

    // Initialization options: forced conformance levers overlaid onto (and
    // winning over) the user's options. Absent both, no `initializationOptions`.
    if let Some(opts) = profile.effective_initialization_options(initialization_options) {
        params["initializationOptions"] = opts;
    }

    params
}

// ── Document synchronization ────────────────────────────────────────

/// Builds `DidOpenTextDocumentParams`.
#[must_use]
pub fn did_open(uri: &str, language_id: &str, version: i32, text: &str) -> Value {
    json!({
        "textDocument": {
            "uri": uri,
            "languageId": language_id,
            "version": version,
            "text": text
        }
    })
}

/// Builds `DidChangeTextDocumentParams` with full content replacement.
#[must_use]
pub fn did_change(uri: &str, version: i32, text: &str) -> Value {
    json!({
        "textDocument": { "uri": uri, "version": version },
        "contentChanges": [{ "text": text }]
    })
}

/// Builds `DidCloseTextDocumentParams`.
#[must_use]
pub fn did_close(uri: &str) -> Value {
    json!({
        "textDocument": { "uri": uri }
    })
}

/// Builds `DidSaveTextDocumentParams` (without included text).
#[must_use]
pub fn did_save(uri: &str) -> Value {
    json!({
        "textDocument": { "uri": uri }
    })
}

// ── Workspace ───────────────────────────────────────────────────────

/// Builds `DidChangeConfigurationParams` with empty settings.
///
/// Pull-model servers will send `workspace/configuration` requests to
/// retrieve updated settings. The empty payload is the standard trigger
/// per the LSP spec.
#[must_use]
pub fn did_change_configuration() -> Value {
    json!({ "settings": {} })
}

/// Builds `DidChangeWatchedFilesParams`.
///
/// `changes` is a slice of `(uri, FileChangeType as u8)` pairs.
#[must_use]
pub fn did_change_watched_files(changes: &[(&str, u8)]) -> Value {
    json!({
        "changes": changes.iter().map(|(uri, typ)| {
            json!({ "uri": uri, "type": typ })
        }).collect::<Vec<_>>()
    })
}

/// Builds `DidChangeWorkspaceFoldersParams`.
///
/// `added` and `removed` are slices of `(uri, name)` pairs.
#[must_use]
pub fn did_change_workspace_folders(added: &[(&str, &str)], removed: &[(&str, &str)]) -> Value {
    json!({
        "event": {
            "added": folder_array(added),
            "removed": folder_array(removed),
        }
    })
}

/// Builds `WorkspaceSymbolParams`.
#[must_use]
pub fn workspace_symbols(query: &str) -> Value {
    json!({ "query": query })
}

// ── Text document requests (position-based) ─────────────────────────

/// Builds `DefinitionParams`.
#[must_use]
pub fn definition(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `TypeDefinitionParams`.
#[must_use]
pub fn type_definition(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `ImplementationParams`.
#[must_use]
pub fn implementation(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `PrepareRenameParams`.
#[must_use]
pub fn prepare_rename(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `ReferenceParams`.
#[must_use]
pub fn references(uri: &str, line: u32, character: u32, include_declaration: bool) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
        "context": { "includeDeclaration": include_declaration }
    })
}

/// Builds `DocumentSymbolParams`.
#[must_use]
pub fn document_symbols(uri: &str) -> Value {
    json!({ "textDocument": { "uri": uri } })
}

// ── Call hierarchy ──────────────────────────────────────────────────

/// Builds `CallHierarchyPrepareParams`.
#[must_use]
pub fn prepare_call_hierarchy(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `CallHierarchyIncomingCallsParams`.
///
/// `item` is a pass-through `CallHierarchyItem` from the prepare response.
#[must_use]
pub fn incoming_calls(item: &Value) -> Value {
    json!({ "item": item })
}

/// Builds `CallHierarchyOutgoingCallsParams`.
///
/// `item` is a pass-through `CallHierarchyItem` from the prepare response.
#[must_use]
pub fn outgoing_calls(item: &Value) -> Value {
    json!({ "item": item })
}

// ── Type hierarchy ──────────────────────────────────────────────────

/// Builds `TypeHierarchyPrepareParams`.
#[must_use]
pub fn prepare_type_hierarchy(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `TypeHierarchySupertypesParams`.
///
/// `item` is a pass-through `TypeHierarchyItem` from the prepare response.
#[must_use]
pub fn supertypes(item: &Value) -> Value {
    json!({ "item": item })
}

// ── Diagnostics ─────────────────────────────────────────────────────

/// Builds `DocumentDiagnosticParams`.
#[must_use]
pub fn text_document_diagnostic(uri: &str) -> Value {
    json!({
        "textDocument": { "uri": uri }
    })
}

/// Builds `WorkspaceDiagnosticParams`.
///
/// `identifier` is the optional `diagnosticProvider.identifier` the server
/// advertised. `previousResultIds` is sent empty for now — a full pull off the
/// server's project model — but the field is present so a future incremental
/// pull can supply cached per-document result IDs without a params change.
#[must_use]
pub fn workspace_diagnostic(identifier: Option<&str>) -> Value {
    let mut params = json!({ "previousResultIds": [] });
    if let Some(id) = identifier {
        params["identifier"] = json!(id);
    }
    params
}

/// Builds `TypeHierarchySubtypesParams`.
///
/// `item` is a pass-through `TypeHierarchyItem` from the prepare response.
#[must_use]
pub fn subtypes(item: &Value) -> Value {
    json!({ "item": item })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── Initialize ──────────────────────────────────────────────────

    #[test]
    fn initialize_single_root() {
        // An un-cased server receives today's capability shape unchanged.
        let ours = initialize(42, &[("file:///workspace", "workspace")], "clangd", None);

        let expected = json!({
            "processId": 42,
            "rootUri": "file:///workspace",
            "capabilities": {
                "general": {
                    "positionEncodings": ["utf-8", "utf-16"]
                },
                "textDocument": {
                    "synchronization": {
                        "didSave": true,
                        "dynamicRegistration": false,
                        "willSave": false,
                        "willSaveWaitUntil": false
                    },
                    "publishDiagnostics": {
                        "versionSupport": true
                    },
                    "definition": {
                        "dynamicRegistration": false,
                        "linkSupport": true
                    },
                    "typeDefinition": {
                        "dynamicRegistration": false,
                        "linkSupport": true
                    },
                    "implementation": {
                        "dynamicRegistration": false,
                        "linkSupport": true
                    },
                    "declaration": {
                        "dynamicRegistration": false,
                        "linkSupport": true
                    },
                    "references": {
                        "dynamicRegistration": false
                    },
                    "documentSymbol": {
                        "dynamicRegistration": false,
                        "hierarchicalDocumentSymbolSupport": true
                    },
                    "callHierarchy": {
                        "dynamicRegistration": false
                    },
                    "typeHierarchy": {
                        "dynamicRegistration": false
                    },
                    "codeAction": {
                        "dynamicRegistration": false
                    },
                    "diagnostic": {
                        "dynamicRegistration": false
                    }
                },
                "workspace": {
                    "symbol": {
                        "resolveSupport": {
                            "properties": ["location.range"]
                        }
                    },
                    "workspaceFolders": false,
                    "configuration": true,
                    "didChangeConfiguration": {
                        "dynamicRegistration": true
                    },
                    "didChangeWatchedFiles": {
                        "dynamicRegistration": true,
                        "relativePatternSupport": true
                    }
                },
                "window": {
                    "workDoneProgress": true
                }
            },
            "workspaceFolders": [
                { "uri": "file:///workspace", "name": "workspace" }
            ]
        });

        assert_eq!(ours, expected);
    }

    #[test]
    fn initialize_capabilities_advertise_did_change_watched_files() {
        let ours = initialize(1, &[("file:///ws", "ws")], "clangd", None);
        let dcwf = &ours["capabilities"]["workspace"]["didChangeWatchedFiles"];
        assert_eq!(dcwf["dynamicRegistration"], json!(true));
        assert_eq!(dcwf["relativePatternSupport"], json!(true));
    }

    #[test]
    fn initialize_capabilities_advertise_did_change_configuration() {
        let ours = initialize(1, &[("file:///ws", "ws")], "clangd", None);
        let dcc = &ours["capabilities"]["workspace"]["didChangeConfiguration"];
        assert_eq!(dcc["dynamicRegistration"], json!(true));
    }

    #[test]
    fn initialize_with_options() {
        let opts = json!({"key": "value"});
        // An un-cased server passes its user options through unchanged.
        let ours = initialize(1, &[("file:///ws", "ws")], "clangd", Some(&opts));

        assert_eq!(ours["processId"], 1);
        assert_eq!(ours["rootUri"], "file:///ws");
        assert_eq!(ours["initializationOptions"], json!({"key": "value"}));
        assert_eq!(
            ours["workspaceFolders"],
            json!([{"uri": "file:///ws", "name": "ws"}])
        );
    }

    #[test]
    fn initialize_suppresses_diagnostic_capability_for_rust_analyzer() {
        // rust-analyzer is cased to not receive the pull client capability.
        let ours = initialize(7, &[("file:///ws", "ws")], "rust-analyzer", None);
        let text_doc = &ours["capabilities"]["textDocument"];
        assert!(
            text_doc.get("diagnostic").is_none(),
            "rust-analyzer must not receive textDocument.diagnostic, got: {text_doc}",
        );
        // Every other textDocument capability stays present — only `diagnostic`
        // is removed.
        assert!(text_doc.get("definition").is_some());
        assert!(text_doc.get("documentSymbol").is_some());
        assert!(text_doc.get("publishDiagnostics").is_some());
    }

    #[test]
    fn initialize_keeps_diagnostic_capability_for_uncased_server() {
        // An un-cased server keeps today's shape, `diagnostic` included.
        let ours = initialize(7, &[("file:///ws", "ws")], "clangd", None);
        assert!(
            ours["capabilities"]["textDocument"]
                .get("diagnostic")
                .is_some(),
            "an un-cased server must still advertise textDocument.diagnostic",
        );
    }

    #[test]
    fn initialize_forces_gopls_conformance_levers() {
        // gopls carries the forced conformance lever even when the user supplies
        // no initializationOptions. Pull is forced OFF (bug 87: pull mode stops
        // real pushes; the empty placeholder publishes read as heard-empty), and
        // the debounce key is enforced absent (run 9 + ruling).
        let ours = initialize(7, &[("file:///ws", "ws")], "gopls", None);
        let opts = &ours["initializationOptions"];
        assert_eq!(opts["pullDiagnostics"], json!(false));
        assert!(opts.get("diagnosticsDelay").is_none());
    }

    #[test]
    fn initialize_gopls_conformance_wins_over_user_options() {
        let user = json!({
            "diagnosticsDelay": "0s",
            "pullDiagnostics": true,
            "buildFlags": ["-tags=x"],
        });
        let ours = initialize(7, &[("file:///ws", "ws")], "gopls", Some(&user));
        let opts = &ours["initializationOptions"];
        // Conformance wins: the pull footgun is overwritten and the delay key
        // is stripped outright (enforced absent — run 9: "0s" decoupled
        // publishing from analysis; only gopls's own default may apply).
        assert_eq!(opts["pullDiagnostics"], json!(false));
        assert!(opts.get("diagnosticsDelay").is_none());
        // The user's unrelated key survives.
        assert_eq!(opts["buildFlags"], json!(["-tags=x"]));
    }

    // ── Document synchronization ────────────────────────────────────

    #[test]
    fn did_open_golden() {
        let ours = did_open("file:///foo.rs", "rust", 1, "fn main() {}");

        assert_eq!(
            ours,
            json!({
                "textDocument": {
                    "uri": "file:///foo.rs",
                    "languageId": "rust",
                    "version": 1,
                    "text": "fn main() {}"
                }
            })
        );
    }

    #[test]
    fn did_change_golden() {
        let ours = did_change("file:///foo.rs", 2, "fn main() { println!() }");

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs", "version": 2 },
                "contentChanges": [{ "text": "fn main() { println!() }" }]
            })
        );
    }

    #[test]
    fn did_close_golden() {
        let ours = did_close("file:///foo.rs");

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" }
            })
        );
    }

    #[test]
    fn did_save_golden() {
        let ours = did_save("file:///foo.rs");

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" }
            })
        );
    }

    // ── Workspace ───────────────────────────────────────────────────

    #[test]
    fn did_change_watched_files_golden() {
        let ours = did_change_watched_files(&[
            ("file:///project/src/new.rs", 1),
            ("file:///project/src/old.rs", 3),
        ]);

        assert_eq!(
            ours,
            json!({
                "changes": [
                    { "uri": "file:///project/src/new.rs", "type": 1 },
                    { "uri": "file:///project/src/old.rs", "type": 3 }
                ]
            })
        );
    }

    #[test]
    fn did_change_watched_files_empty() {
        let ours = did_change_watched_files(&[]);
        assert_eq!(ours, json!({ "changes": [] }));
    }

    #[test]
    fn did_change_configuration_golden() {
        let ours = did_change_configuration();
        assert_eq!(ours, json!({ "settings": {} }));
    }

    #[test]
    fn workspace_symbols_golden() {
        let ours = workspace_symbols("MyStruct");

        assert_eq!(ours, json!({ "query": "MyStruct" }));
    }

    // ── Position-based requests ─────────────────────────────────────

    #[test]
    fn definition_golden() {
        let ours = definition("file:///foo.rs", 10, 5);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 10, "character": 5 }
            })
        );
    }

    #[test]
    fn type_definition_golden() {
        let ours = type_definition("file:///foo.rs", 3, 8);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 3, "character": 8 }
            })
        );
    }

    #[test]
    fn implementation_golden() {
        let ours = implementation("file:///foo.rs", 5, 12);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 5, "character": 12 }
            })
        );
    }

    #[test]
    fn prepare_rename_golden() {
        let ours = prepare_rename("file:///foo.rs", 7, 4);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 7, "character": 4 }
            })
        );
    }

    #[test]
    fn references_golden() {
        let ours = references("file:///foo.rs", 10, 5, true);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 10, "character": 5 },
                "context": { "includeDeclaration": true }
            })
        );
    }

    #[test]
    fn document_symbols_golden() {
        let ours = document_symbols("file:///foo.rs");

        assert_eq!(ours, json!({ "textDocument": { "uri": "file:///foo.rs" } }));
    }

    // ── Call hierarchy ──────────────────────────────────────────────

    fn sample_call_hierarchy_item() -> Value {
        json!({
            "name": "foo",
            "kind": 12,
            "uri": "file:///foo.rs",
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 1, "character": 0 }
            },
            "selectionRange": {
                "start": { "line": 0, "character": 3 },
                "end": { "line": 0, "character": 6 }
            }
        })
    }

    #[test]
    fn prepare_call_hierarchy_golden() {
        let ours = prepare_call_hierarchy("file:///foo.rs", 5, 3);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 5, "character": 3 }
            })
        );
    }

    #[test]
    fn incoming_calls_golden() {
        let item = sample_call_hierarchy_item();
        let ours = incoming_calls(&item);

        assert_eq!(ours, json!({ "item": item }));
    }

    #[test]
    fn outgoing_calls_golden() {
        let item = sample_call_hierarchy_item();
        let ours = outgoing_calls(&item);

        assert_eq!(ours, json!({ "item": item }));
    }

    // ── Diagnostics ──────────────────────────────────────────────────

    #[test]
    fn text_document_diagnostic_golden() {
        let ours = text_document_diagnostic("file:///foo.rs");

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" }
            })
        );
    }

    #[test]
    fn workspace_diagnostic_empty_previous_result_ids() {
        // A full pull with no identifier: only the (empty) previousResultIds.
        assert_eq!(
            workspace_diagnostic(None),
            json!({ "previousResultIds": [] })
        );
    }

    #[test]
    fn workspace_diagnostic_carries_identifier() {
        assert_eq!(
            workspace_diagnostic(Some("rustc")),
            json!({ "previousResultIds": [], "identifier": "rustc" })
        );
    }

    // ── Type hierarchy ──────────────────────────────────────────────

    fn sample_type_hierarchy_item() -> Value {
        json!({
            "name": "MyTrait",
            "kind": 11,
            "uri": "file:///foo.rs",
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 5, "character": 0 }
            },
            "selectionRange": {
                "start": { "line": 0, "character": 6 },
                "end": { "line": 0, "character": 13 }
            }
        })
    }

    #[test]
    fn prepare_type_hierarchy_golden() {
        let ours = prepare_type_hierarchy("file:///foo.rs", 2, 10);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 2, "character": 10 }
            })
        );
    }

    #[test]
    fn supertypes_golden() {
        let item = sample_type_hierarchy_item();
        let ours = supertypes(&item);

        assert_eq!(ours, json!({ "item": item }));
    }

    #[test]
    fn subtypes_golden() {
        let item = sample_type_hierarchy_item();
        let ours = subtypes(&item);

        assert_eq!(ours, json!({ "item": item }));
    }

    #[test]
    fn did_change_workspace_folders_golden() {
        let added = [("file:///sub", "sub")];
        let removed: [(&str, &str); 0] = [];
        let ours = did_change_workspace_folders(&added, &removed);

        assert_eq!(
            ours,
            json!({
                "event": {
                    "added": [{ "uri": "file:///sub", "name": "sub" }],
                    "removed": [],
                }
            })
        );
    }
}
