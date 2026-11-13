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
pub fn generate(
    project_root: &Path,
    out_dir: &Path,
    output_name: &str,
    public_url: &str,
) -> Result<()> {
    generate_with_stylesheet(project_root, out_dir, output_name, None, public_url)
}

pub(crate) fn generate_with_stylesheet(
    project_root: &Path,
    out_dir: &Path,
    output_name: &str,
    stylesheet: Option<&Path>,
    public_url: &str,
) -> Result<()> {
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
    let html = process(&template, output_name, public_url)?;
    let href = stylesheet
        .map(|path| stylesheet_href(out_dir, path, public_url))
        .transpose()?;
    let html = with_stylesheet(&html, href.as_deref())?;
    let html = with_spa_base(&html, public_url)?;
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

fn stylesheet_href(out_dir: &Path, stylesheet: &Path, public_url: &str) -> Result<String> {
    Ok(format!("{public_url}{}", output_url(out_dir, stylesheet)?))
}

/// Publish the browser bootstrap under public/ and private manifest at the build root.
/// URLs in the manifest are rooted at the public output root, never filesystem paths.
pub(crate) fn generate_bootstrap_and_manifest(
    out_dir: &Path,
    public_dir: &Path,
    output_name: &str,
    stylesheet: Option<&Path>,
    public_url: &str,
) -> Result<()> {
    let bootstrap_name = format!("{output_name}.bootstrap.js");
    let js = format!("{output_name}.js");
    let wasm = format!("{output_name}_bg.wasm");
    let js_url = file_url(&js);
    let bootstrap_url = file_url(&bootstrap_name);
    let js_public_url = format!("{public_url}{js_url}");
    let bootstrap_public_url = format!("{public_url}{bootstrap_url}");
    let bootstrap = format!(
        r#"import init from "./{js_url}";
init().catch(error => console.error("Failed to initialize WASM:", error));
"#
    );
    let mut manifest = format!(
        r#"{{
  "bootstrap": "{bootstrap_public_url}",
  "js": "{js_public_url}",
  "wasm": "{public_url}{}",
  "public_url": "{public_url}""#,
        file_url(&wasm)
    );
    if let Some(stylesheet) = stylesheet {
        manifest.push_str(&format!(
            ",\n  \"stylesheet\": \"{public_url}{}\"",
            output_url(public_dir, stylesheet)?
        ));
    }
    manifest.push_str("\n}\n");
    atomic_write(&public_dir.join(bootstrap_name), bootstrap.as_bytes())?;
    atomic_write(&out_dir.join("sycamore-assets.json"), manifest.as_bytes())?;
    Ok(())
}

fn output_url(out_dir: &Path, file: &Path) -> Result<String> {
    let relative = file
        .strip_prefix(out_dir)
        .context("invalid generated stylesheet path")?;
    relative
        .iter()
        .map(|segment| {
            let segment = segment.to_str().context("asset path is not UTF-8")?;
            Ok(utf8_percent_encode(segment, ASSET_PATH).to_string())
        })
        .collect::<Result<Vec<_>>>()
        .map(|segments| segments.join("/"))
}

fn file_url(file: &str) -> String {
    utf8_percent_encode(file, ASSET_PATH).to_string()
}

fn atomic_write(destination: &Path, contents: &[u8]) -> Result<()> {
    let mut output = tempfile::NamedTempFile::new_in(
        destination.parent().context("output file has no parent")?,
    )?;
    output.write_all(contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    output
        .persist(destination)
        .with_context(|| format!("writing {}", destination.display()))?;
    Ok(())
}

fn with_stylesheet(template: &str, href: Option<&str>) -> Result<String> {
    let link = href.map(|href| {
        format!(r#"<link rel="stylesheet" href="{href}" data-cargo-sycamore-tailwind>"#)
    });
    replace_generated_element(
        template,
        link.as_deref(),
        "link[data-cargo-sycamore-tailwind]",
    )
}

fn process(template: &str, output_name: &str, public_url: &str) -> Result<String> {
    let filename = format!("{output_name}.js");
    let filename = utf8_percent_encode(&filename, ASSET_PATH);
    let script = format!(
        r#"<script type="module" data-cargo-sycamore>
import init from "{public_url}{filename}";
init().catch(error => console.error("Failed to initialize WASM:", error));
</script>"#
    );
    append_script(template, &script, "script[data-cargo-sycamore]")
}

/// Inject only into HTTP responses; never add development code to build output.
pub(crate) fn with_reload_client(template: &str, version: &str) -> Result<String> {
    let version = version
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let script = format!(
        r#"<script type="module" src="/__sycamore/reload.js" data-cargo-sycamore-reload data-version="{version}"></script>"#
    );
    append_script(template, &script, "script[data-cargo-sycamore-reload]")
}

/// SPA fallbacks keep root-relative asset resolution on nested browser routes.
/// Honor an explicit user base URL; never write this change to disk.
pub(crate) fn with_spa_base(template: &str, public_url: &str) -> Result<String> {
    let mut document = Html::parse_document(template);
    let base_selector = Selector::parse("head base[href]").expect("valid base selector");
    if document.select(&base_selector).next().is_none() {
        let head_selector = Selector::parse("head").expect("valid head selector");
        let head = document
            .select(&head_selector)
            .next()
            .context("HTML document has no head")?
            .id();
        let fragment = Html::parse_fragment(&format!(r#"<base href="{public_url}">"#));
        let fragment_selector = Selector::parse("base[href]").expect("valid base selector");
        let base = fragment
            .select(&fragment_selector)
            .next()
            .context("could not create SPA base element")?;
        document
            .tree
            .get_mut(head)
            .context("head missing from tree")?
            .prepend(Node::Element(base.value().clone()));
    }
    Ok(document.html())
}

fn append_script(template: &str, script: &str, selector: &str) -> Result<String> {
    replace_generated_element(template, Some(script), selector)
}

fn replace_generated_element(
    template: &str,
    element: Option<&str>,
    selector: &str,
) -> Result<String> {
    // html5ever supplies implicit head/body elements and handles casing,
    // comments, and omitted closing tags without string replacement.
    let mut document = Html::parse_document(template);
    let head_selector = Selector::parse("head").expect("valid head selector");
    let head = document
        .select(&head_selector)
        .next()
        .context("HTML document has no head")?
        .id();
    let generated_selector = Selector::parse(selector)
        .map_err(|_| anyhow::anyhow!("invalid generated-element selector"))?;
    let previous: Vec<_> = document
        .select(&generated_selector)
        .map(|script| script.id())
        .collect();
    for id in previous {
        document
            .tree
            .get_mut(id)
            .context("generated element missing from tree")?
            .detach();
    }
    let Some(element) = element else {
        return Ok(document.html());
    };
    let fragment = Html::parse_fragment(element);
    let script = fragment
        .select(&generated_selector)
        .next()
        .context("could not create generated element")?;
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
    fn generates_tailwind_link_in_head_and_replaces_only_reserved_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let out = root.join("dist");
        fs::create_dir_all(out.join("assets")).unwrap();
        let css = out.join("assets/my styles #%.css");
        fs::write(&css, "body{}").unwrap();
        let template = "<head><link rel=stylesheet href='user.css'><link rel=stylesheet href='old.css' data-cargo-sycamore-tailwind></head><body>App</body>";
        fs::write(root.join("index.html"), template).unwrap();
        generate_with_stylesheet(root, &out, "app", Some(&css), "/my-app/").unwrap();
        let generated = fs::read_to_string(out.join("index.html")).unwrap();
        let document = Html::parse_document(&generated);
        assert_eq!(
            count(&document, "head link[data-cargo-sycamore-tailwind]"),
            1
        );
        assert_eq!(
            first(&document, "link[data-cargo-sycamore-tailwind]")
                .value()
                .attr("href"),
            Some("/my-app/assets/my%20styles%20%23%25.css")
        );
        assert_eq!(
            first(&document, "link[data-cargo-sycamore-tailwind]")
                .value()
                .attr("rel"),
            Some("stylesheet")
        );
        assert_eq!(count(&document, "link[href='user.css']"), 1);
        assert_eq!(count(&document, "link[href='old.css']"), 0);
        assert_eq!(count(&document, "script[data-cargo-sycamore]"), 1);
        assert!(
            text(first(&document, "script[data-cargo-sycamore]"))
                .contains("import init from \"/my-app/app.js\"")
        );
        assert_eq!(count(&document, "base[href='/my-app/']"), 1);
        assert_eq!(
            fs::read_to_string(root.join("index.html")).unwrap(),
            template
        );
        let replaced = with_stylesheet(&generated, Some("./other.css")).unwrap();
        assert_eq!(
            count(
                &Html::parse_document(&replaced),
                "link[data-cargo-sycamore-tailwind]"
            ),
            1
        );
        generate(root, &out, "app", "/").unwrap();
        let disabled = Html::parse_document(&fs::read_to_string(out.join("index.html")).unwrap());
        assert_eq!(count(&disabled, "link[data-cargo-sycamore-tailwind]"), 0);
        assert_eq!(count(&disabled, "link[href='user.css']"), 1);
        fs::remove_file(root.join("index.html")).unwrap();
        generate_with_stylesheet(root, &out, "app", Some(&css), "/my-app/").unwrap();
        assert_eq!(
            count(
                &Html::parse_document(&fs::read_to_string(out.join("index.html")).unwrap()),
                "head link[data-cargo-sycamore-tailwind]"
            ),
            1
        );
    }

    #[test]
    fn publishes_bootstrap_and_root_relative_asset_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("dist");
        let out = directory.join("public");
        fs::create_dir_all(out.join("assets")).unwrap();
        let stylesheet = out.join("assets/site styles.css");
        fs::write(&stylesheet, "body{}").unwrap();

        generate_bootstrap_and_manifest(
            &directory,
            &out,
            "app name",
            Some(&stylesheet),
            "/my-app/",
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(out.join("app name.bootstrap.js")).unwrap(),
            r#"import init from "./app%20name.js";
init().catch(error => console.error("Failed to initialize WASM:", error));
"#
        );
        let manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(directory.join("sycamore-assets.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["bootstrap"], "/my-app/app%20name.bootstrap.js");
        assert_eq!(manifest["js"], "/my-app/app%20name.js");
        assert_eq!(manifest["wasm"], "/my-app/app%20name_bg.wasm");
        assert_eq!(manifest["stylesheet"], "/my-app/assets/site%20styles.css");
        assert_eq!(manifest["public_url"], "/my-app/");
        assert!(!manifest.to_string().contains(out.to_str().unwrap()));
    }

    #[test]
    fn sycamore_app_bootstrap_does_not_replace_bindgen_glue() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path();
        let public = out.join("public");
        fs::create_dir(&public).unwrap();
        let glue = b"// wasm-bindgen generated glue";
        fs::write(public.join("sycamore.js"), glue).unwrap();

        generate_bootstrap_and_manifest(out, &public, "sycamore", None, "/").unwrap();

        assert_eq!(fs::read(public.join("sycamore.js")).unwrap(), glue);
        assert!(public.join("sycamore.bootstrap.js").is_file());
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(out.join("sycamore-assets.json")).unwrap()).unwrap();
        assert_eq!(manifest["bootstrap"], "/sycamore.bootstrap.js");
        assert_eq!(manifest["js"], "/sycamore.js");
    }

    #[test]
    fn rejects_invalid_generated_stylesheet_paths() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("dist");
        fs::create_dir(&out).unwrap();
        let css = temp.path().join("outside.css");
        fs::write(&css, "body{}").unwrap();
        let error =
            generate_with_stylesheet(temp.path(), &out, "app", Some(&css), "/").unwrap_err();
        assert!(format!("{error:#}").contains("invalid generated stylesheet path"));
        assert!(!out.join("index.html").exists());
    }

    #[test]
    fn injects_reload_client_only_into_served_html_and_replaces_previous_client() {
        let built = process(DEFAULT_HTML, "app", "/").unwrap();
        assert!(!built.contains("/__sycamore/reload"));
        let served = with_reload_client(&built, "session-1").unwrap();
        let served = with_reload_client(&served, "session-2").unwrap();
        let document = Html::parse_document(&served);
        assert_eq!(
            count(&document, "head script[data-cargo-sycamore-reload]"),
            1
        );
        let script = first(&document, "head script[data-cargo-sycamore-reload]");
        assert_eq!(script.value().attr("src"), Some("/__sycamore/reload.js"));
        assert_eq!(script.value().attr("data-version"), Some("session-2"));
        assert_eq!(count(&document, "head script[data-cargo-sycamore]"), 1);
        assert!(!built.contains("data-cargo-sycamore-reload"));
    }

    #[test]
    fn reload_version_cannot_escape_its_attribute() {
        let served = with_reload_client(DEFAULT_HTML, "\"><script>alert(1)</script>").unwrap();
        let document = Html::parse_document(&served);
        assert_eq!(count(&document, "script"), 1);
        assert_eq!(
            first(&document, "script").value().attr("data-version"),
            Some("\"><script>alert(1)</script>")
        );
    }

    #[test]
    fn spa_base_is_prepended_and_respects_existing_base() {
        let source = "<head><script type=module>import './app.js';</script></head><body>App</body>";
        let html = with_spa_base(source, "/my-app/").unwrap();
        let document = Html::parse_document(&html);
        assert_eq!(
            first(&document, "head base").value().attr("href"),
            Some("/my-app/")
        );
        assert_eq!(
            first(&document, "head")
                .child_elements()
                .next()
                .unwrap()
                .value()
                .name(),
            "base"
        );
        let html = with_spa_base(&html, "/my-app/").unwrap();
        assert_eq!(count(&Html::parse_document(&html), "base[href]"), 1);
        let html = with_spa_base("<head><base href='/custom/'></head>", "/my-app/").unwrap();
        let document = Html::parse_document(&html);
        assert_eq!(count(&document, "base[href]"), 1);
        assert_eq!(
            first(&document, "base").value().attr("href"),
            Some("/custom/")
        );
    }

    #[test]
    fn default_template_has_a_module_in_head() {
        let html = process(DEFAULT_HTML, "my_app", "/").unwrap();
        let document = Html::parse_document(&html);
        let script = first(&document, "head script[type=module][data-cargo-sycamore]");
        assert!(text(script).contains("import init from \"/my_app.js\""));
        assert!(text(script).contains("init().catch("));
        assert_eq!(count(&document, "meta[charset]"), 1);
    }

    #[test]
    fn preserves_custom_html_and_adds_an_implicit_head() {
        let html = process("<!doctype html><HTML><BODY><main id=app>Hello</main><script src=custom.js></script></BODY></HTML>", "app", "/").unwrap();
        let document = Html::parse_document(&html);
        assert_eq!(text(first(&document, "main#app")), "Hello");
        assert_eq!(count(&document, "head script[data-cargo-sycamore]"), 1);
        assert_eq!(count(&document, "body script[src='custom.js']"), 1);
    }

    #[test]
    fn replaces_previous_generated_script_without_touching_user_scripts() {
        let html = process("<head><!-- </head> --><title>Custom</title><script>window.user = true;</script></head>", "old", "/").unwrap();
        let html = process(&html, "new", "/").unwrap();
        let document = Html::parse_document(&html);
        assert_eq!(count(&document, "script[data-cargo-sycamore]"), 1);
        assert_eq!(count(&document, "head script"), 2);
        assert!(text(first(&document, "head script[data-cargo-sycamore]")).contains("/new.js"));
        assert_eq!(text(first(&document, "title")), "Custom");
        assert!(html.contains("<!-- </head> -->"));
        assert!(html.contains("window.user = true;"));
    }

    #[test]
    fn preserves_doctype_templates_and_raw_script_contents() {
        let html = process(r#"<!doctype html><head><style>body > main { color: red; }</style><script>if (1 < 2 && 3 > 2) window.message = "<&>";</script></head><body><template><span>A &amp; B</span></template></body>"#, "app", "/").unwrap();
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
        let html = process(DEFAULT_HTML, "app\"</script><script>alert(1)</script>", "/").unwrap();
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
        generate(root, &out, "app", "/").unwrap();
        assert!(
            fs::read_to_string(out.join("index.html"))
                .unwrap()
                .contains("Custom app")
        );
        assert_eq!(fs::read_to_string(root.join("index.html")).unwrap(), custom);
        assert!(generate(root, root, "app", "/").is_err());
        fs::remove_file(root.join("index.html")).unwrap();
        generate(root, &out, "app", "/").unwrap();
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
        assert!(generate(temp.path(), temp.path(), "app", "/").is_err());
    }
}
