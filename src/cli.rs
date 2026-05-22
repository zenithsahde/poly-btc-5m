use clap::Parser;

#[derive(Debug, Parser)]
#[command(author, version, about = "SJ Trading Engine")]
pub struct Cli {
    /// 强制 dry-run（即使配置了 [wallet].private_key 也忽略）
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}
