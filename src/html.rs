//! HTML entrypoint generation for browser-targeted wasm-bindgen output.

use std::{
    fs,
    io::{ErrorKind, Write},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use scraper::{Html, Node, Selector};

const DEFAULT_HTML: &str = include_str!("../templates/index.html");
const ASSET_PATH: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.');

/// Use the selected package's index.html, or the bundled default if absent.
/// Writes only to the output directory and refuses to overwrite the source file.
pub fn generate(project_root: &Path, out_dir: &Path, output_name: &str) -> Result<()> {
    let source = project_root.join("index.html");
    let destination = out_dir.join("index.html");
    let template = match fs::read_to_string(&source) {
        Ok(template) => {
            let source_path = fs::canonicalize(&source)?;
            // canonicalize also catches an existing output symlink to the source.
            ensure!(
                fs::canonicalize(&destination).ok().as_ref() != Some(&source_path),
                "HTML output would overwrite source template {}; choose a separate --out-dir",
                source.display()
            );
            template
        }
        Err(error) if error.kind() == ErrorKind::NotFound => DEFAULT_HTML.to_owned(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading HTML template {}", source.display()));
        }
    };
    let html = process(&template, output_name)?;
    let mut output = tempfile::NamedTempFile::new_in(out_dir)?;
    output.write_all(html.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    output
        .persist(&destination)
        .with_context(|| format!("writing {}", destination.display()))?;
    Ok(())
}

fn process(template: &str, output_name: &str) -> Result<String> {
    // HTML5 parsing creates an implicit head/body for fragments and handles
    // casing, comments, and omitted closing tags without string replacement.
    let mut document = Html::parse_document(template);
    let head_selector = Selector::parse("head").expect("valid head selector");
    let head = document
        .select(&head_selector)
        .next()
        .context("HTML document has no head")?
        .id();
    // Replace only our own generated script if the template contains one.
    let generated_selector =
        Selector::parse("script[data-cargo-sycamore]").expect("valid generated-script selector");
    let previous: Vec<_> = document
        .select(&generated_selector)
        .map(|script| script.id())
        .collect();
    for id in previous {
        document
            .tree
            .get_mut(id)
            .context("generated script missing from tree")?
            .detach();
    }
    let filename = format!("{output_name}.js");
    let filename = utf8_percent_encode(&filename, ASSET_PATH);
    let script = format!(
        r#"<script type="module" data-cargo-sycamore>
import init from "./{filename}";
init().catch(error => console.error("Failed to initialize WASM:", error));
</script>"#
    );
    let fragment = Html::parse_fragment(&script);
    let script = fragment
        .select(&generated_selector)
        .next()
        .context("could not create WASM startup script")?;
    let mut head = document
        .tree
        .get_mut(head)
        .context("head missing from tree")?;
    let mut inserted = head.append(Node::Element(script.value().clone()));
    // Script contents are parsed as raw text, not nested HTML elements.
    for child in script.children() {
        inserted.append(child.value().clone());
    }
    Ok(document.html())
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::ElementRef;

    fn first<'a>(document: &'a Html, selector: &str) -> ElementRef<'a> {
        document
            .select(&Selector::parse(selector).unwrap())
            .next()
            .unwrap()
    }

    fn count(document: &Html, selector: &str) -> usize {
        document.select(&Selector::parse(selector).unwrap()).count()
    }

    fn text(element: ElementRef<'_>) -> String {
        element.text().collect()
    }

    #[test]
    fn default_template_has_a_module_in_head() {
        let html = process(DEFAULT_HTML, "my_app").unwrap();
        let document = Html::parse_document(&html);
        let script = first(&document, "head script[type=module][data-cargo-sycamore]");
        assert!(text(script).contains("import init from \"./my_app.js\""));
        assert!(text(script).contains("init().catch("));
        assert_eq!(count(&document, "meta[charset]"), 1);
    }

    #[test]
    fn preserves_custom_html_and_adds_an_implicit_head() {
        let html = process("<!doctype html><HTML><BODY><main id=app>Hello</main><script src=custom.js></script></BODY></HTML>", "app").unwrap();
        let document = Html::parse_document(&html);
        assert_eq!(text(first(&document, "main#app")), "Hello");
        assert_eq!(count(&document, "head script[data-cargo-sycamore]"), 1);
        assert_eq!(count(&document, "body script[src='custom.js']"), 1);
    }

    #[test]
    fn replaces_previous_generated_script_without_touching_user_scripts() {
        let html = process("<head><!-- </head> --><title>Custom</title><script>window.user = true;</script></head>", "old").unwrap();
        let html = process(&html, "new").unwrap();
        let document = Html::parse_document(&html);
        assert_eq!(count(&document, "script[data-cargo-sycamore]"), 1);
        assert_eq!(count(&document, "head script"), 2);
        assert!(text(first(&document, "head script[data-cargo-sycamore]")).contains("./new.js"));
        assert_eq!(text(first(&document, "title")), "Custom");
        assert!(html.contains("<!-- </head> -->"));
        assert!(html.contains("window.user = true;"));
    }

    #[test]
    fn preserves_doctype_templates_and_raw_script_contents() {
        let html = process(r#"<!doctype html><head><style>body > main { color: red; }</style><script>if (1 < 2 && 3 > 2) window.message = "<&>";</script></head><body><template><span>A &amp; B</span></template></body>"#, "app").unwrap();
        let document = Html::parse_document(&html);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert_eq!(
            text(first(&document, "style")),
            "body > main { color: red; }"
        );
        assert_eq!(
            text(first(&document, "script:not([data-cargo-sycamore])")),
            r#"if (1 < 2 && 3 > 2) window.message = "<&>";"#
        );
        assert!(html.contains("<template><span>A &amp; B</span></template>"));
    }

    #[test]
    fn encodes_asset_names_so_they_cannot_escape_the_script() {
        let html = process(DEFAULT_HTML, "app\"</script><script>alert(1)</script>").unwrap();
        let document = Html::parse_document(&html);
        assert_eq!(count(&document, "script"), 1);
        assert!(text(first(&document, "script")).contains("%22%3C%2Fscript%3E"));
    }

    #[test]
    fn chooses_custom_template_without_modifying_it_and_falls_back_when_removed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let out = root.join("dist");
        fs::create_dir(&out).unwrap();
        let custom = "<title>Custom app</title><main>Hello</main>";
        fs::write(root.join("index.html"), custom).unwrap();
        generate(root, &out, "app").unwrap();
        assert!(
            fs::read_to_string(out.join("index.html"))
                .unwrap()
                .contains("Custom app")
        );
        assert_eq!(fs::read_to_string(root.join("index.html")).unwrap(), custom);
        assert!(generate(root, root, "app").is_err());
        fs::remove_file(root.join("index.html")).unwrap();
        generate(root, &out, "app").unwrap();
        assert!(
            fs::read_to_string(out.join("index.html"))
                .unwrap()
                .contains("Sycamore App")
        );
    }

    #[test]
    fn does_not_hide_template_read_errors() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("index.html")).unwrap();
        assert!(generate(temp.path(), temp.path(), "app").is_err());
    }
}
