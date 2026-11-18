// Each fixture exercises the same disk, index, search and diff entry points as the UI.
use crate::{refs, store, view};
use anyhow::Result;
use std::{fs, path::Path};
struct Case {
    source: &'static str,
    body: &'static str,
    target: &'static str,
    target_body: &'static str,
    reference_line: usize,
    definition: Option<usize>,
}
fn check(case: Case) -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "tt-language-{}-{}",
        std::process::id(),
        case.source.replace('/', "_")
    ));
    fs::create_dir_all(&dir)?;
    let root = dir.to_str().unwrap();
    for (path, text) in [(case.source, case.body), (case.target, case.target_body)] {
        let p = dir.join(path);
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(p, text)?;
    }
    let mut db = store::test_database()?;
    let filter = store::rules(&db, root)?;
    store::scan(&mut db, root, &dir, &filter, true)?;
    let forward = refs::links(
        &db,
        root,
        case.source,
        case.body,
        case.reference_line,
        || false,
    )?;
    assert!(
        forward
            .iter()
            .any(|t| t.event.path == case.target && t.label.starts_with("引用")),
        "{} → {}",
        case.source,
        case.target
    );
    if let Some(line) = case.definition {
        assert!(
            forward.iter().any(|t| t.event.path == case.target
                && t.line == line
                && t.label.starts_with("定义候选")),
            "definition in {}",
            case.target
        );
    }
    let back = refs::links(&db, root, case.target, case.target_body, 0, || false)?;
    assert!(
        back.iter().any(|t| t.event.path == case.source
            && t.line == case.reference_line
            && t.label.starts_with("被引用")),
        "return to {}:{}",
        case.source,
        case.reference_line + 1
    );
    let next = case.body.replace("old_marker", "new_marker");
    assert_ne!(next, case.body);
    let path = Path::new(root).join(case.source);
    fs::write(&path, &next)?;
    store::scan(&mut db, root, &path, &filter, false)?;
    let history = store::entries(&db, root, false, case.source, "", 0, 0)?;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].kind, "M");
    let (old, new, _) = store::bodies(&db, history[0].id)?;
    let lines = view::content(old.as_deref(), new.as_deref(), "M", false);
    assert!(
        lines
            .iter()
            .any(|line| line.mark == '~' && line.text.contains("new_marker"))
    );
    assert_eq!(
        store::entries(&db, root, false, case.source, "new_marker", 1, 0)?.len(),
        1
    );
    assert_eq!(
        store::entries(&db, root, false, case.source, "old_marker", 2, 0)?.len(),
        1
    );
    fs::remove_dir_all(dir)?;
    Ok(())
}
#[test]
fn java_import_and_class() -> Result<()> {
    check(Case {
        source: "app/src/Main.java",
        body: "package app;\nimport demo.config.Settings;\nclass Main { String label = \"old_marker\"; }\n",
        target: "lib/src/demo/config/Settings.java",
        target_body: "package demo.config;\npublic class Settings {}\n",
        reference_line: 1,
        definition: Some(1),
    })
}
#[test]
fn python_relative_import() -> Result<()> {
    check(Case {
        source: "pkg/main.py",
        body: "from .settings import Settings\nlabel = 'old_marker'\n",
        target: "pkg/settings.py",
        target_body: "# settings\nclass Settings:\n    pass\n",
        reference_line: 0,
        definition: Some(1),
    })
}
#[test]
fn typescript_extensionless_import() -> Result<()> {
    check(Case {
        source: "src/app.ts",
        body: "import { Settings } from './settings';\nconst label: string = 'old_marker';\n",
        target: "src/settings.ts",
        target_body: "// config\nexport interface Settings {}\n",
        reference_line: 0,
        definition: Some(1),
    })
}
#[test]
fn javascript_require() -> Result<()> {
    check(Case {
        source: "lib/app.js",
        body: "const Settings = require('./settings');\nconst label = 'old_marker';\n",
        target: "lib/settings.js",
        target_body: "// config\nclass Settings {}\n",
        reference_line: 0,
        definition: Some(1),
    })
}
#[test]
fn markdown_relative_link() -> Result<()> {
    check(Case {
        source: "docs/guide.md",
        body: "# old_marker\n[配置](../config/schema.xml)\n",
        target: "config/schema.xml",
        target_body: "<schema/>\n",
        reference_line: 1,
        definition: None,
    })
}
#[test]
fn xml_namespace_include() -> Result<()> {
    check(Case {
        source: "xml/main.xml",
        body: "<config xmlns:xi=\"http://www.w3.org/2001/XInclude\" name=\"old_marker\">\n  <xi:include href=\"../templates/配置.xml\"/>\n</config>\n",
        target: "templates/配置.xml",
        target_body: "<setting/>\n",
        reference_line: 1,
        definition: None,
    })
}
