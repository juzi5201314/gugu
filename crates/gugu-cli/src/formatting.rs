use std::{
    env, fs,
    path::{Path, PathBuf},
};

use gugu_compiler::{Project, format_source};

use crate::{
    GlobalArgs,
    output::{OutputFormat, emit_cli_error},
};

pub(crate) fn run(check: bool, all: bool, options: &GlobalArgs) -> i32 {
    let format = options.format.unwrap_or_default();
    let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project = match Project::discover(&root) {
        Ok(project) => project,
        Err(error) => {
            emit_cli_error(format, &error.to_string());
            return 2;
        }
    };
    let packages = if all {
        project.packages().iter().collect::<Vec<_>>()
    } else {
        match project.current_package() {
            Some(package) => vec![package],
            None => {
                emit_cli_error(format, "当前目录没有 package，请使用 `gugu fmt --all`");
                return 2;
            }
        }
    };
    let mut changed = false;
    for package in packages {
        let mut files = Vec::new();
        if let Err(error) = collect_gg(package.root(), &mut files) {
            emit_cli_error(format, &error);
            return 1;
        }
        files.sort();
        for path in files {
            let relative = match path.strip_prefix(project.workspace().root()) {
                Ok(relative) => relative,
                Err(error) => {
                    emit_cli_error(format, &error.to_string());
                    return 1;
                }
            };
            let source = match fs::read_to_string(&path) {
                Ok(source) => source,
                Err(error) => {
                    emit_cli_error(format, &error.to_string());
                    return 1;
                }
            };
            let rendered = match format_source(relative, &source) {
                Ok(rendered) => rendered,
                Err(error) => {
                    emit_cli_error(format, &error);
                    return 1;
                }
            };
            let differs = source != rendered;
            changed |= differs;
            if differs && !check {
                let temporary = path.with_extension("gg.gugu-fmt-tmp");
                if let Err(error) =
                    fs::write(&temporary, rendered).and_then(|()| fs::rename(&temporary, &path))
                {
                    emit_cli_error(format, &error.to_string());
                    return 1;
                }
            }
            if differs && format == OutputFormat::Text {
                println!("格式差异：{}", relative.display());
            }
            if differs && format == OutputFormat::Json {
                println!(
                    "{{\"event\":\"fmt-diff\",\"path\":{}}}",
                    serde_json::to_string(&relative.display().to_string()).expect("路径可编码")
                );
            }
        }
    }
    if format == OutputFormat::Json {
        println!("{{\"event\":\"fmt-result\",\"changed\":{changed},\"check\":{check}}}");
    }
    if check { i32::from(changed) } else { 0 }
}

fn collect_gg(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| matches!(name, "target" | "vendor" | ".git"))
        {
            continue;
        }
        if entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            collect_gg(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "gg") {
            files.push(path);
        }
    }
    Ok(())
}
