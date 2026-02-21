use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use flate2::write::GzEncoder;
use flate2::Compression;
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use tar::Builder;
use tempfile::TempDir;
use walkdir::WalkDir;

/// 简单的备份/还原工具
///
/// 功能（尽量遵从你要求的流程）：
/// - backup: 将当前目录下的 dump.rdb、config/、data/ 以及 plugins/**/(config|data) 打包到一个 tar.gz
///   - 同时扫描 plugins 下是否有 git 仓库，记录每个仓库的 remote URL 和 分支（以及 commit）到 metadata.json
/// - restore: 从指定的 tar.gz 解包，先按 metadata.json 中的信息把 plugins 的仓库 clone 回正确位置并切换到记录的分支，
///   然后把 config/data/dump.rdb 覆盖回当前目录
#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 备份到一个 tar.gz 文件
    Backup {
        /// 输出文件，例如 backup.tar.gz
        #[arg(short, long)]
        out: PathBuf,
    },
    /// 从备份文件还原
    Restore {
        /// 输入的备份文件
        #[arg(short, long)]
        input: PathBuf,
    },
}

#[derive(Serialize, Deserialize, Debug)]
struct GitRepoInfo {
    /// 相对于工作目录的路径，比如 plugins/foo
    path: String,
    remote: String,
    branch: String,
    commit: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct MetaData {
    repos: Vec<GitRepoInfo>,
    created_at: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Commands::Backup { out } => backup(out).context("backup failed")?,
        Commands::Restore { input } => restore(input).context("restore failed")?,
    }
    Ok(())
}

fn backup(out: PathBuf) -> Result<()> {
    let cwd = std::env::current_dir()?;
    println!("工作目录: {}", cwd.display());

    let mut repos_info: Vec<GitRepoInfo> = Vec::new();

    // 先创建一个 tar builder 写入到 gzip
    let tar_gz = fs::File::create(&out).with_context(|| format!("无法创建输出文件 {}", out.display()))?;
    let enc = GzEncoder::new(tar_gz, Compression::default());
    let mut tar = Builder::new(enc);

    // helper: 尝试把某个路径（文件或目录）添加到 tar
    fn add_path(tar: &mut Builder<GzEncoder<fs::File>>, cwd: &Path, p: &Path) -> Result<()> {
        if p.exists() {
            if p.is_file() {
                tar.append_path_with_name(p, p.strip_prefix(cwd)?.to_path_buf())?;
            } else if p.is_dir() {
                // walkdir 把目录下所有文件加进去
                for entry in WalkDir::new(p) {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_file() {
                        let name = path.strip_prefix(cwd)?;
                        tar.append_path_with_name(path, name)?;
                    }
                }
            }
        }
        Ok(())
    }
    
    // 1) dump.rdb
    let dump = cwd.join("dump.rdb");
    add_path(&mut tar, &cwd, &dump)?;
    
    // 2) ./config 和 ./data
    add_path(&mut tar, &cwd, &cwd.join("config"))?;
    add_path(&mut tar, &cwd, &cwd.join("data"))?;

    // 3) plugins/**/(config|data) 和扫描 git 仓库，并且额外处理 plugins/example 的 .js/.js.bak 与 package.json
    let plugins_dir = cwd.join("plugins");
    if plugins_dir.exists() && plugins_dir.is_dir() {
        for entry in fs::read_dir(&plugins_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                // 加入 plugins/foo/config 和 plugins/foo/data（如果存在）
                add_path(&mut tar, &cwd, &path.join("config"))?;
                add_path(&mut tar, &cwd, &path.join("data"))?;

                // detect git repo inside this plugin folder
                // 如果 path/.git 存在，则尝试打开
                let git_path = path.join(".git");
                if git_path.exists() {
                    if let Ok(repo) = Repository::open(&path) {
                        // remote: try origin first然后其它
                        let mut remote_url = String::new();
                        if let Ok(remote) = repo.find_remote("origin") {
                            if let Some(url) = remote.url() {
                                remote_url = url.to_string();
                            }
                        }
                        if remote_url.is_empty() {
                            if let Ok(remotes) = repo.remotes() {
                                if let Some(name) = remotes.get(0) {
                                    if let Ok(r) = repo.find_remote(name) {
                                        if let Some(url) = r.url() {
                                            remote_url = url.to_string();
                                        }
                                    }
                                }
                            }
                        }

                        let mut branch = String::from("HEAD");
                        if let Ok(head) = repo.head() {
                            if let Some(sh) = head.shorthand() {
                                branch = sh.to_string();
                            }
                        }

                        let commit = repo.head().ok().and_then(|h| h.target()).map(|oid| oid.to_string());

                        repos_info.push(GitRepoInfo {
                            path: path
                                .strip_prefix(&cwd)?
                                .to_string_lossy()
                                .replace('\\', "/"),
                            remote: remote_url,
                            branch,
                            commit,
                        });
                    }
                }

                // --- 新增: 专门处理 plugins/example 下的 .js/.js.bak 文件和 package.json（跳过 node_modules） ---
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if name == "example" {
                        for e in WalkDir::new(&path) {
                            let e = e?;
                            let p = e.path();

                            // 跳过 node_modules 内的任何文件/目录
                            let has_node_modules = p.components().any(|c| c.as_os_str() == OsStr::new("node_modules"));
                            if has_node_modules {
                                continue;
                            }

                            if p.is_file() {
                                let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                                // 包含 .js 或 .js.bak 后缀，或是 package.json
                                if fname.ends_with(".js") || fname.ends_with(".js.bak") || fname == "package.json" {
                                    let name_in_tar = p.strip_prefix(&cwd)?;
                                    tar.append_path_with_name(p, name_in_tar)?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 写 metadata.json 到一个临时文件，再加入到 tar
    let meta = MetaData {
        repos: repos_info,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let meta_json = serde_json::to_vec_pretty(&meta)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(meta_json.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "metadata.json", &meta_json[..])?;

    // finish
    tar.into_inner()?.finish()?;

    println!("已写入备份：{}", out.display());
    Ok(())
}

fn restore(input: PathBuf) -> Result<()> {
    let cwd = std::env::current_dir()?;
    println!("工作目录: {}", cwd.display());

    // 解压到临时目录
    let tmp = TempDir::new()?;
    let tmp_path = tmp.path();

    let tar_gz = fs::File::open(&input).with_context(|| format!("无法打开备份文件 {}", input.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tar_gz));
    archive.unpack(&tmp_path)?;

    // 读取 metadata.json
    let meta_file = tmp_path.join("metadata.json");
    let meta: MetaData = if meta_file.exists() {
        let mut s = String::new();
        fs::File::open(&meta_file)?.read_to_string(&mut s)?;
        serde_json::from_str(&s)?
    } else {
        MetaData { repos: vec![], created_at: "".into() }
    };

    // 1) 按 metadata 先 clone 仓库
    for repo in &meta.repos {
        println!("处理 repo: {} -> {} (branch={})", repo.path, repo.remote, repo.branch);
        if repo.remote.is_empty() {
            println!("  warning: repo {} 没有记录 remote，跳过 clone", repo.path);
            continue;
        }
        let target = cwd.join(&repo.path);
        if target.exists() {
            println!("  目标已存在，先删除：{}", target.display());
            fs::remove_dir_all(&target)?;
        }
        // clone with specified branch
        let mut cb = git2::build::RepoBuilder::new();
        // try to set branch; RepoBuilder::branch expects a branch name
        let rb = if !repo.branch.is_empty() { cb.branch(&repo.branch) } else { &mut cb };
        match rb.clone(&repo.remote, &target) {
            Ok(_) => println!("  clone 完成"),
            Err(e) => println!("  clone 失败: {}", e),
        }
    }

    // 2) 把 tmp 中的 config data dump.rdb 覆盖回 cwd
    // helper copy
    fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
        if !src.exists() {
            return Ok(());
        }
        fs::create_dir_all(dst)?;
        for entry in WalkDir::new(src) {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(src)?;
            let dest_path = dst.join(rel);
            if path.is_dir() {
                fs::create_dir_all(&dest_path)?;
            } else if path.is_file() {
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(path, &dest_path)?;
            }
        }
        Ok(())
    }

    // config
    let tmp_config = tmp_path.join("config");
    copy_dir_all(&tmp_config, &cwd.join("config"))?;
    // data
    let tmp_data = tmp_path.join("data");
    copy_dir_all(&tmp_data, &cwd.join("data"))?;

    // plugins/**/config 和 data，以及单独还原 plugins/example 下的 .js/.js.bak 与 package.json
    let tmp_plugins = tmp_path.join("plugins");
    if tmp_plugins.exists() {
        for entry in fs::read_dir(&tmp_plugins)? {
            let entry = entry?;
            let plugin_path = entry.path();
            if plugin_path.is_dir() {
                let rel = plugin_path.strip_prefix(&tmp_plugins)?;
                let dst_plugin = cwd.join("plugins").join(rel);
                // 先还原 config 与 data（如果存在）
                copy_dir_all(&plugin_path.join("config"), &dst_plugin.join("config"))?;
                copy_dir_all(&plugin_path.join("data"), &dst_plugin.join("data"))?;

                // 如果是 example 插件，单独遍历并还原 .js/.js.bak 与 package.json（跳过 node_modules）
                if let Some(name) = plugin_path.file_name().and_then(|s| s.to_str()) {
                    if name == "example" {
                        // 确保目标目录存在
                        fs::create_dir_all(&dst_plugin)?;
                        for e in WalkDir::new(&plugin_path) {
                            let e = e?;
                            let p = e.path();

                            // 跳过 node_modules 内的任何文件/目录
                            let has_node_modules = p.components().any(|c| c.as_os_str() == OsStr::new("node_modules"));
                            if has_node_modules {
                                continue;
                            }

                            if p.is_file() {
                                let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                                if fname.ends_with(".js") || fname.ends_with(".js.bak") || fname == "package.json" {
                                    let relp = p.strip_prefix(&plugin_path)?;
                                    let dest = dst_plugin.join(relp);
                                    if let Some(parent) = dest.parent() {
                                        fs::create_dir_all(parent)?;
                                    }
                                    // 直接覆盖写入（package.json 也会覆盖）
                                    fs::copy(p, &dest)?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // dump.rdb
    let tmp_dump = tmp_path.join("dump.rdb");
    if tmp_dump.exists() {
        fs::copy(&tmp_dump, &cwd.join("dump.rdb"))?;
        println!("dump.rdb 已恢复到 {}", cwd.join("dump.rdb").display());
    }

    println!("还原完成");
    Ok(())
}