//! Collects the translatable texts of the project into `po/zh_CN.po`.
//!
//! Sources: `gettext!("...")` calls in the Rust sources and the `translatable`
//! texts of the GTK templates. Existing translations are kept by their English
//! text, new texts are added with an empty translation. Run from the repository
//! root with `./tools/update-po.sh`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let root = PathBuf::from(root);
    let mut sources: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for file in walk(&root.join("src")) {
        collect_rust(&root, &file, &mut sources);
    }
    for dir in ["lan-mouse-cli", "lan-mouse-ipc", "lan-mouse-gtk"] {
        for file in walk(&root.join(dir).join("src")) {
            collect_rust(&root, &file, &mut sources);
        }
    }
    for file in walk(&root.join("lan-mouse-gtk").join("resources")) {
        if file.extension().is_some_and(|e| e == "ui") {
            collect_ui(&root, &file, &mut sources);
        }
    }
    // messages written in an error attribute, they are not calls
    for text in ["no display is attached to this Windows session (monitor switched on and connected?)"] {
        sources
            .entry(text.to_owned())
            .or_insert_with(|| vec!["input-capture/src/error.rs".to_owned()]);
    }
    // subcommand descriptions of the main binary, they live in attributes
    for text in [
        "Lan Mouse commandline interface",
        "run in daemon mode",
        "run the lan-mouse service in the foreground",
        "Runs the lan-mouse service without the GTK frontend, meant for headless machines and scripts.",
    ] {
        sources
            .entry(text.to_owned())
            .or_insert_with(|| vec!["src/config.rs, lan-mouse-cli/src/lib.rs".to_owned()]);
    }
    // clap's own labels, written by the command line help of the CLI
    for label in [
        // `Usage:` is translated as the whole help template in the CLI crate
        "Usage: {usage}\n\n{all-args}",
        "Commands",
        "Arguments",
        "Options",
        "Print help",
        "Print version",
    ] {
        sources
            .entry(label.to_owned())
            .or_insert_with(|| vec!["lan-mouse-cli/src/lib.rs (clap label)".to_owned()]);
    }

    let known = read_catalogue(&root.join("po/zh_CN.po"));
    let mut out = String::new();
    out.push_str(
        "# Simplified Chinese translation for lan-mouse.\n\
         # This file is distributed under the same license as the lan-mouse package.\n\
         # Regenerate with ./tools/update-po.sh, translations are kept by their English text.\n\
         msgid \"\"\n\
         msgstr \"\"\n\
         \"Project-Id-Version: lan-mouse\\n\"\n\
         \"Language: zh_CN\\n\"\n\
         \"MIME-Version: 1.0\\n\"\n\
         \"Content-Type: text/plain; charset=UTF-8\\n\"\n\
         \"Content-Transfer-Encoding: 8bit\\n\"\n\
         \"Plural-Forms: nplurals=1; plural=0;\\n\"\n\n",
    );
    let mut missing = Vec::new();
    for (msgid, refs) in &sources {
        out.push_str(&format!("#: {}\n", refs.join(" ")));
        out.push_str(&format!("msgid \"{}\"\n", escape(msgid)));
        let translation = known.get(msgid).cloned().unwrap_or_default();
        if translation.is_empty() {
            missing.push(msgid.clone());
        }
        out.push_str(&format!("msgstr \"{}\"\n\n", escape(&translation)));
    }
    fs::write(root.join("po/zh_CN.po"), out).expect("write catalogue");
    println!(
        "{} messages, {} without a translation",
        sources.len(),
        missing.len()
    );
    for msgid in missing {
        println!("  untranslated: {msgid}");
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// `gettext!("text")` and `gettext("text")` calls
fn collect_rust(root: &Path, file: &Path, sources: &mut BTreeMap<String, Vec<String>>) {
    if file.extension().is_none_or(|e| e != "rs") {
        return;
    }
    let text = fs::read_to_string(file).unwrap_or_default();
    let reference = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    let bytes = text.as_bytes();
    // the macro call and the plain function, `i18n::gettext` or `gettext`
    let needles: [&[u8]; 2] = [b"gettext!(\"", b"gettext(\""];
    let mut i = 0;
    while i + 4 < bytes.len() {
        let Some(offset) = needles
            .iter()
            .filter_map(|n| find(&bytes[i..], n).map(|p| (p, n.len())))
            .min_by_key(|(p, _)| *p)
        else {
            break;
        };
        let start = i + offset.0 + offset.1;
        // the string ends at the first unescaped quote
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'"' {
            if bytes[end] == b'\\' {
                end += 2;
            } else if bytes[end] == b'{' {
                // a `{placeholder}` may contain quotes of its own
                end += placeholder_end(&bytes[end..]) + 1;
            } else {
                end += 1;
            }
        }
        let literal = unescape(&text[start..end]);
        // the usage example and doc comments are no messages
        // the usage example in a doc comment is no message
        if literal.is_empty()
            || literal.contains("some English text")
            || reference.ends_with("src/i18n.rs")
        {
            i = end + 1;
            continue;
        }
        sources
            .entry(literal)
            .or_default()
            .push(format!("{reference}"));
        i = end + 1;
    }
}

/// offset of the closing brace of the `{placeholder}` starting at `text[0]`
fn placeholder_end(text: &[u8]) -> usize {
    text.iter().position(|b| *b == b'}').unwrap_or(usize::MAX)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `translatable="yes">text</property>` in GTK templates
fn collect_ui(root: &Path, file: &Path, sources: &mut BTreeMap<String, Vec<String>>) {
    let text = fs::read_to_string(file).unwrap_or_default();
    let reference = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    // `translatable="yes">text</attribute>` and `...>` on the next line
    for (index, line) in text.lines().enumerate() {
        let Some(start) = line.find("translatable=\"yes\">") else {
            continue;
        };
        let rest = &line[start + "translatable=\"yes\">".len()..];
        let end = rest.find('<').or_else(|| {
            // the text runs over the end of the line
            text.lines()
                .skip(index + 1)
                .find_map(|next| next.find('<').map(|e| e))
                .map(|e| rest.len() + 1 + e)
        });
        let Some(end) = end else {
            continue;
        };
        let full: String = std::iter::once(rest)
            .chain(text.lines().skip(index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        let content = &full[..end];
        if content.trim().is_empty() || content.trim_start().starts_with("<!--") {
            continue;
        }
        // markup double escapes the ampersand, the parser reads it once
        let content = content.replace("&#38;", "&");
        sources
            .entry(xml_unescape(&content))
            .or_default()
            .push(reference.clone());
    }
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// msgid -> msgstr of the existing catalogue
fn read_catalogue(path: &Path) -> BTreeMap<String, String> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut map = BTreeMap::new();
    // [msgid, msgstr]; empty when no entry is being read
    let mut current: [String; 2] = [String::new(), String::new()];
    let mut in_str = false;
    let mut in_msgid = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            if !in_str && !in_msgid && !current[0].is_empty() {
                map.insert(current[0].clone(), current[1].clone());
                current = [String::new(), String::new()];
            }
            in_str = false;
            in_msgid = false;
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("msgid \"") {
            if !current[0].is_empty() {
                map.insert(current[0].clone(), current[1].clone());
                current = [String::new(), String::new()];
            }
            current[0] = unescape_str(rest);
            in_msgid = true;
        } else if let Some(rest) = line.strip_prefix("msgstr \"") {
            in_msgid = false;
            current[1] = unescape_str(rest);
            in_str = true;
        } else if line.starts_with('"') && in_str {
            current[1].push_str(&unescape_str(line));
        } else if line.starts_with('"') && in_msgid {
            current[0].push_str(&unescape_str(line));
        }
    }
    // the header entry has an empty msgid and is not needed
    if !current[0].is_empty() {
        map.insert(current[0].clone(), current[1].clone());
    }
    map
}

/// the text of `"...\"` including the closing quote
fn unescape_str(rest: &str) -> String {
    let inner = rest.strip_prefix('"').unwrap_or(rest);
    unescape(inner.strip_suffix('"').unwrap_or(inner))
}
