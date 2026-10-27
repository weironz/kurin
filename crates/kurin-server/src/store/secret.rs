//! Secret sentinel handling (UI §6.6).
//!
//! Configuration revisions are stored verbatim (v1 trust model: the database
//! is 0600, the same level as the `cluster.toml` the CLI reads). The API,
//! however, never echoes a secret: [`redact`] replaces every literal secret
//! value with [`SECRET_SENTINEL`] before a document leaves the store, and
//! [`restore`] puts the previous revision's value back when the client saves
//! that sentinel — so "read, edit, write back" neither loses nor duplicates
//! a password.
//!
//! Secret fields (UI §6.6 plus the sudo password, which is a secret too):
//!
//! - `ssh.auth.password`, `ssh.auth.passphrase`, `ssh.become-password`
//! - `registry.password`
//! - the same three under every `nodes.<name>` override.
//!
//! Secret *references* (`{ env = "..." }`, `{ file = "..." }`) are not
//! secrets themselves and are left untouched. Both `[ssh.auth]` tables and
//! the inline form `auth = { method = "password", password = "..." }` are
//! handled.

use toml_edit::{DocumentMut, Item, Value};

use super::StoreError;

/// The literal the API returns in place of every secret value.
pub const SECRET_SENTINEL: &str = "<kurin:secret>";

/// Replace every literal secret value with [`SECRET_SENTINEL`].
pub(crate) fn redact(doc: &mut DocumentMut) {
    for path in secret_paths(doc) {
        let literal = lookup(doc, &path)
            .and_then(value_of)
            .is_some_and(|value| value.as_str().is_some());
        if literal {
            set_value(doc, &path, Value::from(SECRET_SENTINEL));
        }
    }
}

/// Restore every sentinel in `doc` from the previous revision.
///
/// A sentinel with no previous value is an error: silently storing the
/// sentinel would destroy the password.
pub(crate) fn restore(
    doc: &mut DocumentMut,
    previous: Option<&DocumentMut>,
) -> Result<(), StoreError> {
    for path in secret_paths(doc) {
        let submitted = lookup(doc, &path)
            .and_then(value_of)
            .and_then(Value::as_str)
            == Some(SECRET_SENTINEL);
        if !submitted {
            continue;
        }
        let restored = previous
            .and_then(|prev| lookup(prev, &path))
            .and_then(value_of)
            .cloned();
        match restored {
            Some(value) if value.as_str() != Some(SECRET_SENTINEL) => {
                set_value(doc, &path, value);
            }
            _ => {
                return Err(StoreError::MissingSecret {
                    path: display_path(&path),
                });
            }
        }
    }
    Ok(())
}

/// Every secret slot in the document, including per-node overrides.
fn secret_paths(doc: &DocumentMut) -> Vec<Vec<String>> {
    let mut paths = vec![
        path(&["ssh", "auth", "password"]),
        path(&["ssh", "auth", "passphrase"]),
        path(&["ssh", "become-password"]),
        path(&["registry", "password"]),
    ];
    for node in keys_at(doc, &path(&["nodes"])) {
        paths.push(vec![
            "nodes".into(),
            node.clone(),
            "auth".into(),
            "password".into(),
        ]);
        paths.push(vec![
            "nodes".into(),
            node.clone(),
            "auth".into(),
            "passphrase".into(),
        ]);
        paths.push(vec!["nodes".into(), node, "become-password".into()]);
    }
    paths
}

fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

fn display_path(path: &[String]) -> String {
    path.join(".")
}

// ── cursor: tables and inline tables look the same ─────────────────────
//
// `toml_edit` exposes a table's children as `Item`s but an inline table's
// children as `Value`s. A cursor abstracts the two so the same path walk
// works for `[ssh.auth]` and `auth = { ... }`.

enum Cursor<'a> {
    Item(&'a Item),
    Value(&'a Value),
}

enum CursorMut<'a> {
    Item(&'a mut Item),
    Value(&'a mut Value),
}

fn child<'a>(cursor: Cursor<'a>, key: &str) -> Option<Cursor<'a>> {
    match cursor {
        Cursor::Item(Item::Table(table)) => table.get(key).map(Cursor::Item),
        Cursor::Item(Item::Value(value)) => child(Cursor::Value(value), key),
        Cursor::Item(_) => None,
        Cursor::Value(Value::InlineTable(table)) => table.get(key).map(Cursor::Value),
        Cursor::Value(_) => None,
    }
}

fn child_mut<'a>(cursor: CursorMut<'a>, key: &str) -> Option<CursorMut<'a>> {
    match cursor {
        CursorMut::Item(item) => match item {
            Item::Table(table) => table.get_mut(key).map(CursorMut::Item),
            Item::Value(value) => child_mut(CursorMut::Value(value), key),
            _ => None,
        },
        CursorMut::Value(value) => match value {
            Value::InlineTable(table) => table.get_mut(key).map(CursorMut::Value),
            _ => None,
        },
    }
}

fn lookup<'a>(doc: &'a DocumentMut, path: &[String]) -> Option<Cursor<'a>> {
    let mut cursor = Cursor::Item(doc.as_item());
    for key in path {
        cursor = child(cursor, key)?;
    }
    Some(cursor)
}

fn lookup_mut<'a>(doc: &'a mut DocumentMut, path: &[String]) -> Option<CursorMut<'a>> {
    let mut cursor = CursorMut::Item(doc.as_item_mut());
    for key in path {
        cursor = child_mut(cursor, key)?;
    }
    Some(cursor)
}

fn value_of(cursor: Cursor<'_>) -> Option<&Value> {
    match cursor {
        Cursor::Item(Item::Value(value)) => Some(value),
        Cursor::Value(value) => Some(value),
        Cursor::Item(_) => None,
    }
}

fn set_value(doc: &mut DocumentMut, path: &[String], value: Value) -> bool {
    match lookup_mut(doc, path) {
        Some(CursorMut::Item(item)) => {
            *item = Item::Value(value);
            true
        }
        Some(CursorMut::Value(slot)) => {
            *slot = value;
            true
        }
        None => false,
    }
}

fn keys_at(doc: &DocumentMut, path: &[String]) -> Vec<String> {
    match lookup(doc, path) {
        Some(Cursor::Item(Item::Table(table))) => {
            table.iter().map(|(key, _)| key.to_string()).collect()
        }
        Some(Cursor::Value(Value::InlineTable(table))) => {
            table.iter().map(|(key, _)| key.to_string()).collect()
        }
        _ => Vec::new(),
    }
}
