//! The agent-facing action vocabulary: structured `{action, target, name, value, …}`
//! payloads and their compilation into [`EditOp`]s. Target narrowing (`body` /
//! `return` / `param.N` / `doc`) happens here; range resolution happens in the apply
//! handlers against the structure tree.
use ci_core::{EditOp, Error, Result};

/// Structured rich action payload (the MCP/wrapper input shape):
/// `{action, target, name, value, old_text?, new_text?}`.
#[derive(Debug, Clone, Default)]
pub struct Action {
    pub path: String,
    pub action: String,
    pub target: Option<String>,
    pub name: Option<String>,
    pub value: Option<String>,
    /// For `replace_text`: the exact substring to replace (must be unique within the target node).
    pub old_text: Option<String>,
    /// For `replace_text`: its replacement.
    pub new_text: Option<String>,
}

/// Map a structured action to an [`EditOp`]. `resolve(path, target, name) -> node_id`
/// turns target-kind + name addressing into a node id (via the structure tree).
pub fn action_to_op(
    a: &Action,
    resolve: impl Fn(&str, Option<&str>, Option<&str>) -> Option<String>,
) -> Result<EditOp> {
    let node = || {
        resolve(&a.path, a.target.as_deref(), a.name.as_deref())
            .ok_or_else(|| Error::Anchor(format!("{}#{}", a.path, a.name.clone().unwrap_or_default())))
    };
    // The resolved symbol id, optionally NARROWED to a sub-node anchor when `target` names one
    // (`body` / `return` / `param.N`). For a surgical edit the agent targets a sub-symbol range
    // — its body or return type or one parameter — instead of re-emitting the whole definition.
    // An UNRECOGNIZED target is an error, never a silent fallthrough: falling back to the whole
    // symbol would apply sub-node code (a body, a type) over the entire declaration — a
    // silently-wrong edit is worse than one clear retry. The narrowed id (`f.ts#foo:body`) is
    // validated against the structure tree in `apply_structural`.
    let targeted = || -> Result<String> {
        let base = node()?;
        Ok(match a.target.as_deref() {
            None | Some("") => base,
            Some("body") => format!("{base}:body"),
            Some("return") | Some("returnType") => format!("{base}:return"),
            Some("doc") | Some("comment") | Some("docstring") => format!("{base}:doc"),
            Some(t) if t.starts_with("param.") && t["param.".len()..].parse::<u32>().is_ok() => {
                format!("{base}:{t}")
            }
            Some(t) => {
                return Err(Error::Other(format!(
                    "unknown target {t:?} — use `body`, `return`, `doc`, or `param.N` (0-based), or omit \
                     `target` to address the whole symbol"
                )))
            }
        })
    };
    let value = || a.value.clone().ok_or_else(|| Error::Other(format!("{} needs a value", a.action)));
    Ok(match a.action.as_str() {
        "rename" => EditOp::Rename { node_id: node()?, new_name: value()? },
        "replace_node" => EditOp::ReplaceNode { node_id: targeted()?, code: value()? },
        // `replace_text` swaps an exact substring INSIDE a node (optionally a sub-node via
        // `target`) — the cheapest precise edit: the agent sends only old→new, not the whole
        // body. `old_text` must be unique within the node. Gated like any structural edit.
        "replace_text" => EditOp::ReplaceText {
            node_id: targeted()?,
            old_text: a.old_text.clone().ok_or_else(|| Error::Other("replace_text needs oldText".into()))?,
            new_text: a.new_text.clone().ok_or_else(|| Error::Other("replace_text needs newText".into()))?,
        },
        // `set_body` is sugar for replacing the `:body` anchor — re-draft a function/method body
        // without retyping its signature. Gated like any other structural edit.
        "set_body" => EditOp::SetBody { node_id: node()?, body: value()? },
        "insert_before" => EditOp::InsertBefore { node_id: targeted()?, code: value()? },
        // Statement-level body edits. `value` is the statement; `oldText` locates a line inside the
        // body (the `after` anchor for insert; the statement to remove for delete).
        "insert_in_body" => {
            EditOp::InsertInBody { node_id: node()?, code: value()?, after: a.old_text.clone() }
        }
        "insert_member" => EditOp::InsertMember { node_id: node()?, code: value()? },
        "delete_in_body" => EditOp::DeleteInBody {
            node_id: node()?,
            text: a.old_text.clone().or_else(|| a.value.clone()).ok_or_else(|| {
                Error::Other("delete_in_body needs oldText (the statement fragment to remove)".into())
            })?,
        },
        // Signature edits at an insertion point (no existing sub-node anchor). `value` is the new
        // parameter / return type.
        "add_parameter" => EditOp::AddParameter { node_id: node()?, param: value()? },
        "set_return_type" => EditOp::SetReturnType { node_id: node()?, ty: value()? },
        // `add_symbol` appends a NEW top-level declaration at the end of `path` — the intent
        // "add a function/type/test to this file", which insert_before can't express (it
        // needs an existing LATER anchor). Addressing is the file; the code carries the name.
        "add_symbol" => {
            if a.path.is_empty() {
                return Err(Error::Other("add_symbol needs `path` (the file to append to)".into()));
            }
            EditOp::AddSymbol { path: a.path.clone().into(), code: value()? }
        }
        "create_file" => EditOp::CreateFile { path: a.path.clone().into(), code: value()? },
        "move_file" => EditOp::MoveFile { from: a.path.clone().into(), to: value()?.into() },
        "delete_file" => EditOp::DeleteFile { path: a.path.clone().into() },
        other => {
            return Err(Error::Driver(format!(
                "unsupported action {other:?} — valid actions: rename, replace_text, replace_node, set_body, \
                 insert_in_body, delete_in_body, insert_member, add_parameter, set_return_type, insert_before, \
                 add_symbol, move_file, create_file, delete_file"
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_maps_set_body_and_subnode_targets() {
        let resolve = |_p: &str, _t: Option<&str>, n: Option<&str>| n.map(|n| format!("a.ts#{n}"));
        let act = |action: &str, target: Option<&str>| {
            action_to_op(
                &Action {
                    path: "a.ts".into(),
                    action: action.into(),
                    target: target.map(str::to_string),
                    name: Some("add".into()),
                    value: Some("v".into()),
                    ..Default::default()
                },
                resolve,
            )
        };

        // rename targets the whole symbol regardless of `target`.
        assert!(matches!(act("rename", Some("function")).unwrap(), EditOp::Rename { .. }));

        // set_body maps to SET_BODY against the symbol (apply_structural narrows to `:body`).
        match act("set_body", None).unwrap() {
            EditOp::SetBody { node_id, .. } => assert_eq!(node_id, "a.ts#add"),
            o => panic!("expected SetBody, got {o:?}"),
        }

        // replace_node narrows to the sub-node anchor when `target` names one.
        let id = |op| match op {
            EditOp::ReplaceNode { node_id, .. } => node_id,
            o => panic!("expected ReplaceNode, got {o:?}"),
        };
        assert_eq!(id(act("replace_node", Some("body")).unwrap()), "a.ts#add:body");
        assert_eq!(id(act("replace_node", Some("return")).unwrap()), "a.ts#add:return");
        assert_eq!(id(act("replace_node", Some("param.1")).unwrap()), "a.ts#add:param.1");
        assert_eq!(id(act("replace_node", Some("doc")).unwrap()), "a.ts#add:doc");
        // an unknown target is REJECTED (it used to fall through to the whole symbol — which
        // silently applied sub-node code over the entire declaration); no target = whole symbol.
        assert!(act("replace_node", Some("function")).is_err());
        assert_eq!(id(act("replace_node", None).unwrap()), "a.ts#add");

        // replace_text carries oldText/newText and honors `target` for sub-node scoping.
        let rt = action_to_op(
            &Action {
                path: "a.ts".into(),
                action: "replace_text".into(),
                target: Some("body".into()),
                name: Some("add".into()),
                old_text: Some("foo".into()),
                new_text: Some("bar".into()),
                ..Default::default()
            },
            resolve,
        )
        .unwrap();
        match rt {
            EditOp::ReplaceText { node_id, old_text, new_text } => {
                assert_eq!(node_id, "a.ts#add:body");
                assert_eq!(old_text, "foo");
                assert_eq!(new_text, "bar");
            }
            o => panic!("expected ReplaceText, got {o:?}"),
        }
    }

    #[test]
    fn action_maps_file_ops() {
        let resolve = |_p: &str, _t: Option<&str>, _n: Option<&str>| None;
        let mv = action_to_op(
            &Action { path: "a.ts".into(), action: "move_file".into(), target: None, name: None, value: Some("b/a.ts".into()), ..Default::default() },
            resolve,
        )
        .unwrap();
        assert!(matches!(mv, EditOp::MoveFile { .. }));
        let del = action_to_op(
            &Action { path: "a.ts".into(), action: "delete_file".into(), target: None, name: None, value: None, ..Default::default() },
            resolve,
        )
        .unwrap();
        assert!(matches!(del, EditOp::DeleteFile { .. }));
    }

    #[test]
    fn unknown_target_is_rejected_never_widened() {
        let resolve = |_: &str, _: Option<&str>, _: Option<&str>| Some("a.ts#foo".to_string());
        let act = |target: Option<&str>| Action {
            path: String::new(),
            action: "replace_node".into(),
            target: target.map(str::to_string),
            name: Some("foo".into()),
            value: Some("x".into()),
            old_text: None,
            new_text: None,
        };
        // A bogus target must error — falling back to the whole symbol would apply sub-node code
        // over the entire declaration.
        for bad in ["function", "params", "param 1", "param.x", "first"] {
            let err = action_to_op(&act(Some(bad)), resolve).unwrap_err().to_string();
            assert!(err.contains("unknown target"), "target {bad:?} must be rejected, got: {err}");
        }
        // Valid targets narrow to the sub-node; none/empty stays the whole symbol.
        for (t, want) in [("body", "a.ts#foo:body"), ("returnType", "a.ts#foo:return"), ("param.1", "a.ts#foo:param.1")] {
            match action_to_op(&act(Some(t)), resolve).unwrap() {
                EditOp::ReplaceNode { node_id, .. } => assert_eq!(node_id, want),
                other => panic!("unexpected op: {other:?}"),
            }
        }
        for whole in [None, Some("")] {
            match action_to_op(&act(whole), resolve).unwrap() {
                EditOp::ReplaceNode { node_id, .. } => assert_eq!(node_id, "a.ts#foo"),
                other => panic!("unexpected op: {other:?}"),
            }
        }
    }
}
