use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};

const FASTMAIL_SESSION_URL: &str = "https://api.fastmail.com/jmap/session";

#[derive(Debug, Parser)]
#[command(name = "mailbridge", version)]
#[command(
    about = "Mailbox bridge over gws Gmail profiles and Fastmail JMAP with draft-only writes"
)]
struct Cli {
    #[arg(long, global = true, help = "Emit machine-readable JSON")]
    json: bool,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    #[command(about = "Show configured accounts")]
    Accounts,
    #[command(about = "Check backend availability and auth state")]
    Doctor,
    #[command(about = "Search one mailbox")]
    Search(SearchArgs),
    #[command(about = "Read one message")]
    Read(ReadArgs),
    #[command(about = "Read one thread")]
    Thread(ThreadArgs),
    #[command(subcommand, about = "List or download message attachments")]
    Attachments(AttachmentsCommand),
    #[command(subcommand, about = "Create provider drafts without sending")]
    Draft(DraftCommand),
    #[command(about = "Pull messages and attachments into a local evidence directory")]
    Pull(PullArgs),
}

#[derive(Debug, Subcommand)]
enum AttachmentsCommand {
    #[command(about = "List attachments for one message")]
    List(AttachmentListArgs),
    #[command(about = "Download attachments for one message")]
    Download(AttachmentDownloadArgs),
}

#[derive(Debug, Subcommand)]
enum DraftCommand {
    #[command(about = "Create a draft message from a JSON spec without sending")]
    Create(DraftCreateArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum AccountKind {
    Personal,
    Domain,
    Fastmail,
}

#[derive(Debug, Args)]
struct SearchArgs {
    #[arg(long, value_enum)]
    account: AccountKind,
    #[arg(help = "Gmail query for Gmail accounts. Text query for Fastmail.")]
    query: String,
    #[arg(long, default_value_t = 10)]
    limit: usize,
}

#[derive(Debug, Args)]
struct ReadArgs {
    #[arg(long, value_enum)]
    account: AccountKind,
    #[arg(long, aliases = ["id"])]
    message_id: String,
}

#[derive(Debug, Args)]
struct ThreadArgs {
    #[arg(long, value_enum)]
    account: AccountKind,
    #[arg(long, aliases = ["id"])]
    thread_id: String,
    #[arg(
        long,
        default_value_t = 200,
        help = "Fastmail only: recent messages to scan for matching thread id"
    )]
    scan_limit: usize,
}

#[derive(Debug, Args)]
struct PullArgs {
    #[arg(long, value_enum)]
    account: AccountKind,
    #[arg(long)]
    query: Option<String>,
    #[arg(long)]
    message_id: Option<String>,
    #[arg(long)]
    thread_id: Option<String>,
    #[arg(long, default_value_t = 10)]
    limit: usize,
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(
        long,
        help = "Use first search result when query matches multiple Gmail threads"
    )]
    select_first: bool,
    #[arg(long, default_value_t = 200, help = "Fastmail thread scan limit")]
    scan_limit: usize,
    #[arg(long, default_value_t = true, action = clap::ArgAction::SetTrue, conflicts_with = "no_body", help = "Include message body text/html in messages.json")]
    include_body: bool,
    #[arg(long, action = clap::ArgAction::SetTrue, help = "Skip message body text/html in messages.json")]
    no_body: bool,
    #[arg(long, help = "Open the output folder after a successful pull")]
    open: bool,
}

#[derive(Debug, Args)]
struct AttachmentListArgs {
    #[arg(long, value_enum)]
    account: AccountKind,
    #[arg(long, aliases = ["id"])]
    message_id: String,
}

#[derive(Debug, Args)]
struct AttachmentDownloadArgs {
    #[arg(long, value_enum)]
    account: AccountKind,
    #[arg(long, aliases = ["id"])]
    message_id: String,
    #[arg(long)]
    out: PathBuf,
}

#[derive(Debug, Args)]
struct DraftCreateArgs {
    #[arg(long, value_enum)]
    account: AccountKind,
    #[arg(long, help = "Path to a draft JSON spec")]
    spec: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Account {
    id: AccountKind,
    email: String,
    backend: String,
    config: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct MailbridgeConfig {
    accounts: Option<ConfiguredAccounts>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ConfiguredAccounts {
    personal: Option<AccountOverride>,
    domain: Option<AccountOverride>,
    fastmail: Option<AccountOverride>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AccountOverride {
    email: Option<String>,
    config: Option<String>,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    accounts: Vec<AccountDoctor>,
}

#[derive(Debug, Serialize)]
struct AccountDoctor {
    account: Account,
    available: bool,
    auth_ok: bool,
    user: Option<String>,
    token_valid: Option<bool>,
    scopes: Vec<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct MessageSummary {
    account: AccountKind,
    backend: String,
    message_id: String,
    thread_id: Option<String>,
    subject: Option<String>,
    from: Option<String>,
    to: Vec<String>,
    date: Option<String>,
    snippet: Option<String>,
    attachment_names: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct MessageDetail {
    summary: MessageSummary,
    body_text: Option<String>,
    body_html: Option<String>,
    attachments: Vec<AttachmentMeta>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AttachmentMeta {
    filename: String,
    mime_type: Option<String>,
    size_bytes: Option<u64>,
    attachment_id: Option<String>,
    blob_id: Option<String>,
    output_path: Option<String>,
    sha256: Option<String>,
}

#[derive(Debug, Serialize)]
struct AttachmentList {
    account: AccountKind,
    message_id: String,
    attachments: Vec<AttachmentMeta>,
}

#[derive(Debug, Serialize)]
struct AttachmentDownloadManifest {
    account: AccountKind,
    message_id: String,
    out_dir: String,
    attachment_count: usize,
    attachments: Vec<AttachmentMeta>,
}

#[derive(Debug, Serialize)]
struct ThreadDetail {
    account: AccountKind,
    backend: String,
    thread_id: String,
    messages: Vec<MessageDetail>,
}

#[derive(Debug, Serialize)]
struct PullManifest {
    account: AccountKind,
    backend: String,
    selection: String,
    out_dir: String,
    run_path: String,
    messages_path: String,
    evidence_path: String,
    attachments_dir: String,
    messages_count: usize,
    attachment_count: usize,
    message_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct DraftSpec {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    bcc: Vec<String>,
    #[serde(default, alias = "replyTo")]
    reply_to: Vec<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default, alias = "body", alias = "bodyText", alias = "body_text")]
    text: Option<String>,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    attachments: Vec<PathBuf>,
    #[serde(default)]
    screenshots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct PreparedDraft {
    account: Account,
    from: String,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    reply_to: Vec<String>,
    subject: String,
    text: Option<String>,
    html: Option<String>,
    files: Vec<DraftFile>,
}

#[derive(Debug, Clone)]
struct DraftFile {
    kind: DraftAttachmentKind,
    source_path: PathBuf,
    filename: String,
    mime_type: String,
    bytes: Vec<u8>,
    sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum DraftAttachmentKind {
    Attachment,
    Screenshot,
}

#[derive(Debug)]
struct DraftProviderResult {
    draft_id: String,
    message_id: Option<String>,
    thread_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct DraftCreateManifest {
    account: AccountKind,
    backend: String,
    draft_id: String,
    message_id: Option<String>,
    thread_id: Option<String>,
    subject: String,
    to: Vec<String>,
    cc: Vec<String>,
    reply_to: Vec<String>,
    bcc_count: usize,
    attachment_count: usize,
    screenshot_count: usize,
    attachments: Vec<DraftCreatedAttachment>,
    sent: bool,
}

#[derive(Debug, Serialize)]
struct DraftCreatedAttachment {
    kind: DraftAttachmentKind,
    path: String,
    filename: String,
    mime_type: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug)]
struct ParsedEmailAddress {
    name: Option<String>,
    email: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Accounts => output(cli.json, &accounts(), render_accounts(&accounts())),
        CliCommand::Doctor => {
            let report = DoctorReport {
                accounts: accounts().into_iter().map(check_account).collect(),
            };
            output(cli.json, &report, render_doctor(&report))
        }
        CliCommand::Search(args) => {
            let messages = search_account(args.account, &args.query, args.limit)?;
            output(cli.json, &messages, render_search(&messages))
        }
        CliCommand::Read(args) => {
            let message = read_account(args.account, &args.message_id, true)?;
            output(cli.json, &message, render_read(&message))
        }
        CliCommand::Thread(args) => {
            let thread = thread_account(args.account, &args.thread_id, args.scan_limit, true)?;
            output(cli.json, &thread, render_thread(&thread))
        }
        CliCommand::Attachments(AttachmentsCommand::List(args)) => {
            let attachments = list_attachments(args.account, &args.message_id)?;
            output(cli.json, &attachments, render_attachment_list(&attachments))
        }
        CliCommand::Attachments(AttachmentsCommand::Download(args)) => {
            let manifest = download_attachments(args.account, &args.message_id, &args.out)?;
            output(cli.json, &manifest, render_attachment_download(&manifest))
        }
        CliCommand::Draft(DraftCommand::Create(args)) => {
            let manifest = create_draft(args)?;
            output(cli.json, &manifest, render_draft_create(&manifest))
        }
        CliCommand::Pull(args) => {
            ensure_pull_selector(&args)?;
            let manifest = pull_account(args)?;
            output(cli.json, &manifest, render_pull(&manifest))
        }
    }
}

fn accounts() -> Vec<Account> {
    let config = load_config().ok();
    vec![
        configured_account(
            AccountKind::Personal,
            "personal",
            "gmail-gws",
            default_personal_gws_config_dir(),
            config
                .as_ref()
                .and_then(|config| config.accounts.as_ref())
                .and_then(|accounts| accounts.personal.clone()),
        ),
        configured_account(
            AccountKind::Domain,
            "domain",
            "gmail-gws",
            default_domain_gws_config_dir(),
            config
                .as_ref()
                .and_then(|config| config.accounts.as_ref())
                .and_then(|accounts| accounts.domain.clone()),
        ),
        configured_account(
            AccountKind::Fastmail,
            "fastmail",
            "fastmail-jmap",
            default_fastmail_env_path(),
            config
                .as_ref()
                .and_then(|config| config.accounts.as_ref())
                .and_then(|accounts| accounts.fastmail.clone()),
        ),
    ]
}

fn configured_account(
    id: AccountKind,
    default_email: &str,
    backend: &str,
    default_config: PathBuf,
    account_override: Option<AccountOverride>,
) -> Account {
    Account {
        id,
        email: account_override
            .as_ref()
            .and_then(|account| account.email.clone())
            .unwrap_or_else(|| default_email.to_string()),
        backend: backend.to_string(),
        config: account_override
            .and_then(|account| account.config)
            .unwrap_or_else(|| default_config.display().to_string()),
    }
}

fn load_config() -> Result<MailbridgeConfig> {
    let path = config_path();
    if !path.exists() {
        return Ok(MailbridgeConfig { accounts: None });
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

fn config_path() -> PathBuf {
    if let Some(path) = env::var_os("MAILBRIDGE_CONFIG") {
        return PathBuf::from(path);
    }
    home_dir().join(".config/mailbridge/config.json")
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_personal_gws_config_dir() -> PathBuf {
    home_dir().join(".config/gws")
}

fn default_domain_gws_config_dir() -> PathBuf {
    home_dir().join(".config/gws-domain")
}

fn default_fastmail_env_path() -> PathBuf {
    home_dir().join(".config/mailbridge/fastmail.env")
}

fn account(kind: AccountKind) -> Account {
    accounts()
        .into_iter()
        .find(|account| account.id == kind)
        .expect("known account")
}

fn output<T: Serialize>(json_mode: bool, value: &T, text: String) -> Result<()> {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{text}");
    }
    Ok(())
}

fn render_accounts(accounts: &[Account]) -> String {
    accounts
        .iter()
        .map(|account| {
            format!(
                "{:?}\n  email: {}\n  backend: {}\n  config: {}",
                account.id, account.email, account.backend, account.config
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_doctor(report: &DoctorReport) -> String {
    report
        .accounts
        .iter()
        .map(|doctor| {
            let status = if doctor.auth_ok { "ok" } else { "not ready" };
            let mut line = format!(
                "{:?}: {status}\n  backend: {}\n  config: {}\n  user: {}",
                doctor.account.id,
                doctor.account.backend,
                doctor.account.config,
                doctor.user.as_deref().unwrap_or("-")
            );
            if let Some(error) = &doctor.error {
                line.push_str(&format!("\n  error: {error}"));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_search(messages: &[MessageSummary]) -> String {
    if messages.is_empty() {
        return "No matching messages found.".to_string();
    }
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            format!(
                "{}. {}\n   message: {}\n   thread: {}\n   from: {}\n   date: {}\n   attachments: {}",
                index + 1,
                message.subject.as_deref().unwrap_or("(no subject)"),
                message.message_id,
                message.thread_id.as_deref().unwrap_or("-"),
                message.from.as_deref().unwrap_or("-"),
                message.date.as_deref().unwrap_or("-"),
                if message.attachment_names.is_empty() {
                    "none".to_string()
                } else {
                    message.attachment_names.join(", ")
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_read(message: &MessageDetail) -> String {
    format!(
        "{}\nfrom: {}\ndate: {}\nmessage: {}\nthread: {}\n\n{}",
        message.summary.subject.as_deref().unwrap_or("(no subject)"),
        message.summary.from.as_deref().unwrap_or("-"),
        message.summary.date.as_deref().unwrap_or("-"),
        message.summary.message_id,
        message.summary.thread_id.as_deref().unwrap_or("-"),
        message.body_text.as_deref().unwrap_or("")
    )
}

fn render_thread(thread: &ThreadDetail) -> String {
    format!(
        "{} thread {}: {} messages",
        thread.backend,
        thread.thread_id,
        thread.messages.len()
    )
}

fn render_attachment_list(list: &AttachmentList) -> String {
    if list.attachments.is_empty() {
        return format!("No attachments found for message {}.", list.message_id);
    }
    list.attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            format!(
                "{}. {}\n   mime: {}\n   size: {}",
                index + 1,
                attachment.filename,
                attachment.mime_type.as_deref().unwrap_or("-"),
                attachment
                    .size_bytes
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "-".to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_attachment_download(manifest: &AttachmentDownloadManifest) -> String {
    format!(
        "Downloaded {} attachments\nout: {}",
        manifest.attachment_count, manifest.out_dir
    )
}

fn render_draft_create(manifest: &DraftCreateManifest) -> String {
    format!(
        "Created draft {}\naccount: {:?}\nsubject: {}\nattachments: {}\nsent: false",
        manifest.draft_id, manifest.account, manifest.subject, manifest.attachment_count
    )
}

fn render_pull(manifest: &PullManifest) -> String {
    format!(
        "Pulled {} messages and {} attachments\nrun: {}\nmessages: {}\nevidence: {}\nattachments: {}",
        manifest.messages_count,
        manifest.attachment_count,
        manifest.run_path,
        manifest.messages_path,
        manifest.evidence_path,
        manifest.attachments_dir
    )
}

fn ensure_pull_selector(args: &PullArgs) -> Result<()> {
    let count = args.query.is_some() as u8
        + args.message_id.is_some() as u8
        + args.thread_id.is_some() as u8;
    if count != 1 {
        bail!("choose exactly one selector: --query, --message-id, or --thread-id");
    }
    Ok(())
}

fn check_account(account: Account) -> AccountDoctor {
    match account.id {
        AccountKind::Personal | AccountKind::Domain => check_gmail_account(account),
        AccountKind::Fastmail => check_fastmail_account(account),
    }
}

fn check_gmail_account(account: Account) -> AccountDoctor {
    let mut doctor = AccountDoctor {
        account: account.clone(),
        available: command_available("gws"),
        auth_ok: false,
        user: None,
        token_valid: None,
        scopes: Vec::new(),
        error: None,
    };
    if !doctor.available {
        doctor.error = Some("gws not found on PATH".to_string());
        return doctor;
    }
    match run_gws(Path::new(&account.config), ["auth", "status"]) {
        Ok(raw) => match parse_json_output::<GwsAuthStatus>(&raw) {
            Ok(status) => {
                doctor.user = status.user;
                doctor.token_valid = status.token_valid;
                doctor.scopes = status.scopes.unwrap_or_default();
                doctor.auth_ok = doctor.token_valid == Some(true) && doctor.user.is_some();
            }
            Err(error) => doctor.error = Some(error.to_string()),
        },
        Err(error) => doctor.error = Some(error.to_string()),
    }
    doctor
}

fn check_fastmail_account(account: Account) -> AccountDoctor {
    let mut doctor = AccountDoctor {
        account: account.clone(),
        available: Path::new(&account.config).exists(),
        auth_ok: false,
        user: None,
        token_valid: None,
        scopes: Vec::new(),
        error: None,
    };
    match FastmailClient::new(Path::new(&account.config)) {
        Ok(client) => match client.session() {
            Ok(session) => {
                doctor.user = session.username.clone();
                doctor.token_valid = Some(true);
                doctor.auth_ok = true;
            }
            Err(error) => doctor.error = Some(error.to_string()),
        },
        Err(error) => doctor.error = Some(error.to_string()),
    }
    doctor
}

fn search_account(account: AccountKind, query: &str, limit: usize) -> Result<Vec<MessageSummary>> {
    match account {
        AccountKind::Personal | AccountKind::Domain => search_gmail(account, query, limit),
        AccountKind::Fastmail => search_fastmail(query, limit),
    }
}

fn read_account(
    account: AccountKind,
    message_id: &str,
    include_attachment_payloads: bool,
) -> Result<MessageDetail> {
    match account {
        AccountKind::Personal | AccountKind::Domain => {
            read_gmail(account, message_id, include_attachment_payloads)
        }
        AccountKind::Fastmail => read_fastmail(message_id, include_attachment_payloads),
    }
}

fn thread_account(
    account: AccountKind,
    thread_id: &str,
    scan_limit: usize,
    include_body: bool,
) -> Result<ThreadDetail> {
    match account {
        AccountKind::Personal | AccountKind::Domain => {
            thread_gmail(account, thread_id, include_body)
        }
        AccountKind::Fastmail => thread_fastmail(thread_id, scan_limit, include_body),
    }
}

fn pull_account(args: PullArgs) -> Result<PullManifest> {
    let include_body = args.include_body && !args.no_body;
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| default_run_dir(args.account));
    fs::create_dir_all(&out_dir).with_context(|| format!("create {}", out_dir.display()))?;
    let attachments_dir = out_dir.join("attachments");
    fs::create_dir_all(&attachments_dir)
        .with_context(|| format!("create {}", attachments_dir.display()))?;

    let (selection, mut messages) = resolve_pull_messages(&args)?;
    if !include_body {
        for message in &mut messages {
            message.body_text = None;
            message.body_html = None;
        }
    }
    let mut attachment_count = 0usize;
    for (index, message) in messages.iter_mut().enumerate() {
        let materialized = materialize_attachments(
            args.account,
            &message.summary.message_id,
            index,
            &message.attachments,
            &attachments_dir,
        )?;
        attachment_count += materialized.len();
        message.attachments = materialized;
    }

    let messages_path = out_dir.join("messages.json");
    let evidence_path = out_dir.join("evidence.md");
    let run_path = out_dir.join("run.json");
    write_json_file(&messages_path, &messages)?;
    write_evidence(&evidence_path, args.account, &selection, &messages)?;

    let manifest = PullManifest {
        account: args.account,
        backend: account(args.account).backend.to_string(),
        selection,
        out_dir: out_dir.display().to_string(),
        run_path: run_path.display().to_string(),
        messages_path: messages_path.display().to_string(),
        evidence_path: evidence_path.display().to_string(),
        attachments_dir: attachments_dir.display().to_string(),
        messages_count: messages.len(),
        attachment_count,
        message_ids: messages
            .iter()
            .map(|message| message.summary.message_id.clone())
            .collect(),
    };
    write_json_file(&run_path, &manifest)?;
    if args.open {
        open_path(&out_dir)?;
    }
    Ok(manifest)
}

fn list_attachments(account: AccountKind, message_id: &str) -> Result<AttachmentList> {
    let message = read_account(account, message_id, true)?;
    Ok(AttachmentList {
        account,
        message_id: message_id.to_string(),
        attachments: message.attachments,
    })
}

fn download_attachments(
    account: AccountKind,
    message_id: &str,
    out_dir: &Path,
) -> Result<AttachmentDownloadManifest> {
    fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;
    let message = read_account(account, message_id, true)?;
    let attachments = materialize_attachments(
        account,
        &message.summary.message_id,
        0,
        &message.attachments,
        out_dir,
    )?;
    Ok(AttachmentDownloadManifest {
        account,
        message_id: message_id.to_string(),
        out_dir: out_dir.display().to_string(),
        attachment_count: attachments.len(),
        attachments,
    })
}

fn create_draft(args: DraftCreateArgs) -> Result<DraftCreateManifest> {
    let spec = read_draft_spec(&args.spec)?;
    let draft = prepare_draft(args.account, &spec)?;
    let provider = match args.account {
        AccountKind::Personal | AccountKind::Domain => create_gmail_draft(args.account, &draft)?,
        AccountKind::Fastmail => create_fastmail_draft(&draft)?,
    };
    let attachments = draft
        .files
        .iter()
        .map(|file| DraftCreatedAttachment {
            kind: file.kind,
            path: file.source_path.display().to_string(),
            filename: file.filename.clone(),
            mime_type: file.mime_type.clone(),
            size_bytes: file.bytes.len() as u64,
            sha256: file.sha256.clone(),
        })
        .collect::<Vec<_>>();
    Ok(DraftCreateManifest {
        account: args.account,
        backend: draft.account.backend.clone(),
        draft_id: provider.draft_id,
        message_id: provider.message_id,
        thread_id: provider.thread_id,
        subject: draft.subject,
        to: draft.to,
        cc: draft.cc,
        reply_to: draft.reply_to,
        bcc_count: draft.bcc.len(),
        attachment_count: draft.files.len(),
        screenshot_count: draft
            .files
            .iter()
            .filter(|file| file.kind == DraftAttachmentKind::Screenshot)
            .count(),
        attachments,
        sent: false,
    })
}

fn read_draft_spec(path: &Path) -> Result<DraftSpec> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

fn prepare_draft(account_kind: AccountKind, spec: &DraftSpec) -> Result<PreparedDraft> {
    let acct = account(account_kind);
    let from = spec
        .from
        .clone()
        .unwrap_or_else(|| acct.email.clone())
        .trim()
        .to_string();
    validate_address("from", &from)?;
    validate_address_list("to", &spec.to)?;
    validate_address_list("cc", &spec.cc)?;
    validate_address_list("bcc", &spec.bcc)?;
    validate_address_list("reply_to", &spec.reply_to)?;
    if spec.to.is_empty() && spec.cc.is_empty() && spec.bcc.is_empty() {
        bail!("draft spec must include at least one to, cc, or bcc recipient");
    }
    let subject = validate_header_value("subject", spec.subject.as_deref().unwrap_or(""))?;
    let text = non_empty_body(spec.text.as_deref());
    let html = non_empty_body(spec.html.as_deref());
    if text.is_none() && html.is_none() {
        bail!("draft spec must include non-empty text or html body content");
    }
    let files = load_draft_files(&spec.attachments, &spec.screenshots)?;
    Ok(PreparedDraft {
        account: acct,
        from,
        to: spec.to.clone(),
        cc: spec.cc.clone(),
        bcc: spec.bcc.clone(),
        reply_to: spec.reply_to.clone(),
        subject,
        text,
        html,
        files,
    })
}

fn non_empty_body(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn load_draft_files(attachments: &[PathBuf], screenshots: &[PathBuf]) -> Result<Vec<DraftFile>> {
    let mut files = Vec::new();
    for path in attachments {
        files.push(load_draft_file(path, DraftAttachmentKind::Attachment)?);
    }
    for path in screenshots {
        files.push(load_draft_file(path, DraftAttachmentKind::Screenshot)?);
    }
    Ok(files)
}

fn load_draft_file(path: &Path, kind: DraftAttachmentKind) -> Result<DraftFile> {
    if !path.is_absolute() {
        bail!(
            "draft attachment paths must be absolute, got {}",
            path.display()
        );
    }
    let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !metadata.is_file() {
        bail!("draft attachment is not a file: {}", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("attachment path has no valid file name: {}", path.display()))?
        .to_string();
    validate_header_value("attachment filename", &filename)?;
    let mime_type = infer_mime_type(path);
    let sha256 = sha256_bytes(&bytes);
    Ok(DraftFile {
        kind,
        source_path: path.to_path_buf(),
        filename,
        mime_type,
        bytes,
        sha256,
    })
}

fn create_gmail_draft(
    account_kind: AccountKind,
    draft: &PreparedDraft,
) -> Result<DraftProviderResult> {
    ensure_gmail_draft_scope(Path::new(&draft.account.config))?;
    let mime = build_mime_message(draft)?;
    let mime_path = write_gmail_upload_mime(&mime)?;
    let mime_path_arg = mime_path.to_string_lossy().to_string();
    let params = json!({ "userId": "me" });
    let response = run_gws(
        Path::new(&draft.account.config),
        [
            "gmail",
            "users",
            "drafts",
            "create",
            "--params",
            &params.to_string(),
            "--upload",
            &mime_path_arg,
            "--upload-content-type",
            "message/rfc822",
            "--json",
            "{}",
        ],
    );
    let cleanup = fs::remove_file(&mime_path)
        .with_context(|| format!("remove Gmail draft upload {}", mime_path.display()));
    let response = response.with_context(|| format!("create Gmail draft for {:?}", account_kind))?;
    cleanup?;
    let created: GwsDraftResponse = parse_json_output(&response)?;
    Ok(DraftProviderResult {
        draft_id: created.id,
        message_id: created
            .message
            .as_ref()
            .and_then(|message| message.id.clone()),
        thread_id: created.message.and_then(|message| message.thread_id),
    })
}

fn write_gmail_upload_mime(mime: &str) -> Result<PathBuf> {
    let cwd = env::current_dir().with_context(|| "read current directory for Gmail draft upload")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let path = cwd.join(format!(
        ".mailbridge-draft-upload-{}-{nonce}.eml",
        std::process::id()
    ));
    fs::write(&path, mime).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn ensure_gmail_draft_scope(config_dir: &Path) -> Result<()> {
    let raw = run_gws(config_dir, ["auth", "status"])?;
    let status: GwsAuthStatus = parse_json_output(&raw)?;
    let scopes = status.scopes.unwrap_or_default();
    if scopes.iter().any(|scope| {
        matches!(
            scope.as_str(),
            "https://mail.google.com/"
                | "https://www.googleapis.com/auth/gmail.modify"
                | "https://www.googleapis.com/auth/gmail.compose"
        )
    }) {
        return Ok(());
    }
    bail!(
        "Gmail draft creation requires gmail.compose, gmail.modify, or mail.google.com scope. Current scopes: {}. Re-authenticate with: GOOGLE_WORKSPACE_CLI_CONFIG_DIR={} gws auth login --services gmail --scopes https://www.googleapis.com/auth/gmail.readonly,https://www.googleapis.com/auth/gmail.compose",
        if scopes.is_empty() {
            "none".to_string()
        } else {
            scopes.join(",")
        },
        shell_quote(&config_dir.display().to_string())
    )
}

fn create_fastmail_draft(draft: &PreparedDraft) -> Result<DraftProviderResult> {
    let client = FastmailClient::new(Path::new(&draft.account.config))?;
    client.create_draft(draft)
}

fn build_mime_message(draft: &PreparedDraft) -> Result<String> {
    let mut out = String::new();
    out.push_str(&format!(
        "From: {}\r\n",
        validate_header_value("from", &draft.from)?
    ));
    push_address_header(&mut out, "To", &draft.to)?;
    push_address_header(&mut out, "Cc", &draft.cc)?;
    push_address_header(&mut out, "Bcc", &draft.bcc)?;
    push_address_header(&mut out, "Reply-To", &draft.reply_to)?;
    out.push_str(&format!(
        "Subject: {}\r\n",
        validate_header_value("subject", &draft.subject)?
    ));
    out.push_str(&format!("Date: {}\r\n", chrono::Utc::now().to_rfc2822()));
    out.push_str("MIME-Version: 1.0\r\n");

    if draft.files.is_empty() {
        push_mime_body(&mut out, draft)?;
    } else {
        let mixed_boundary = mime_boundary("mixed");
        out.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{mixed_boundary}\"\r\n\r\n"
        ));
        out.push_str(&format!("--{mixed_boundary}\r\n"));
        push_mime_body(&mut out, draft)?;
        for file in &draft.files {
            out.push_str(&format!("--{mixed_boundary}\r\n"));
            push_mime_attachment(&mut out, file)?;
        }
        out.push_str(&format!("--{mixed_boundary}--\r\n"));
    }
    Ok(out)
}

fn push_mime_body(out: &mut String, draft: &PreparedDraft) -> Result<()> {
    match (draft.text.as_deref(), draft.html.as_deref()) {
        (Some(text), Some(html)) => {
            let boundary = mime_boundary("alternative");
            out.push_str(&format!(
                "Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n"
            ));
            out.push_str(&format!("--{boundary}\r\n"));
            push_text_part(out, "text/plain", text);
            out.push_str(&format!("--{boundary}\r\n"));
            push_text_part(out, "text/html", html);
            out.push_str(&format!("--{boundary}--\r\n\r\n"));
        }
        (Some(text), None) => push_text_part(out, "text/plain", text),
        (None, Some(html)) => push_text_part(out, "text/html", html),
        (None, None) => bail!("draft body is empty"),
    }
    Ok(())
}

fn push_text_part(out: &mut String, mime_type: &str, body: &str) {
    out.push_str(&format!(
        "Content-Type: {mime_type}; charset=\"UTF-8\"\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{}\r\n\r\n",
        normalize_body(body)
    ));
}

fn push_mime_attachment(out: &mut String, file: &DraftFile) -> Result<()> {
    let filename = escape_quoted_header_value(&file.filename)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&file.bytes);
    out.push_str(&format!(
        "Content-Type: {}; name=\"{}\"\r\nContent-Disposition: attachment; filename=\"{}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n",
        file.mime_type, filename, filename
    ));
    out.push_str(&wrap_base64(&encoded));
    out.push_str("\r\n");
    Ok(())
}

fn push_address_header(out: &mut String, name: &str, addresses: &[String]) -> Result<()> {
    if addresses.is_empty() {
        return Ok(());
    }
    let values = addresses
        .iter()
        .map(|address| validate_header_value(name, address))
        .collect::<Result<Vec<_>>>()?;
    out.push_str(&format!("{name}: {}\r\n", values.join(", ")));
    Ok(())
}

fn validate_address_list(label: &str, addresses: &[String]) -> Result<()> {
    for address in addresses {
        validate_address(label, address)?;
    }
    Ok(())
}

fn validate_address(label: &str, address: &str) -> Result<()> {
    let value = validate_header_value(label, address)?;
    let parsed = parse_email_address(&value)?;
    if parsed.email.is_empty() || !parsed.email.contains('@') {
        bail!("{label} must contain a valid email address");
    }
    Ok(())
}

fn validate_header_value(label: &str, value: &str) -> Result<String> {
    if value.contains('\r') || value.contains('\n') {
        bail!("{label} cannot contain newline characters");
    }
    Ok(value.trim().to_string())
}

fn escape_quoted_header_value(value: &str) -> Result<String> {
    let value = validate_header_value("quoted header value", value)?;
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn parse_email_address(value: &str) -> Result<ParsedEmailAddress> {
    let trimmed = validate_header_value("email address", value)?;
    if let Some(start) = trimmed.rfind('<')
        && trimmed.ends_with('>')
        && start < trimmed.len() - 1
    {
        let name = trimmed[..start].trim().trim_matches('"').trim();
        let email = trimmed[start + 1..trimmed.len() - 1].trim();
        return Ok(ParsedEmailAddress {
            name: (!name.is_empty()).then(|| name.to_string()),
            email: email.to_string(),
        });
    }
    Ok(ParsedEmailAddress {
        name: None,
        email: trimmed,
    })
}

fn jmap_email_addresses(addresses: &[String]) -> Result<Vec<Value>> {
    addresses
        .iter()
        .map(|address| {
            let parsed = parse_email_address(address)?;
            Ok(json!({
                "name": parsed.name,
                "email": parsed.email
            }))
        })
        .collect()
}

fn normalize_body(body: &str) -> String {
    body.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n")
}

fn wrap_base64(encoded: &str) -> String {
    let mut out = String::new();
    let mut start = 0;
    while start < encoded.len() {
        let end = (start + 76).min(encoded.len());
        out.push_str(&encoded[start..end]);
        out.push_str("\r\n");
        start = end;
    }
    out
}

fn mime_boundary(prefix: &str) -> String {
    format!(
        "mailbridge-{prefix}-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    )
}

fn infer_mime_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("md") => "text/markdown",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("html") | Some("htm") => "text/html",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn jmap_body_structure(
    draft: &PreparedDraft,
    uploaded: &[FastmailUploadResponse],
) -> (Value, Value) {
    let mut body_values = serde_json::Map::new();
    let body_part = match (draft.text.as_deref(), draft.html.as_deref()) {
        (Some(text), Some(html)) => {
            body_values.insert("text".to_string(), json!({ "value": text }));
            body_values.insert("html".to_string(), json!({ "value": html }));
            json!({
                "type": "multipart/alternative",
                "subParts": [
                    {
                        "partId": "text",
                        "type": "text/plain",
                        "charset": "utf-8",
                        "disposition": "inline",
                        "size": text.len()
                    },
                    {
                        "partId": "html",
                        "type": "text/html",
                        "charset": "utf-8",
                        "disposition": "inline",
                        "size": html.len()
                    }
                ]
            })
        }
        (Some(text), None) => {
            body_values.insert("text".to_string(), json!({ "value": text }));
            json!({
                "partId": "text",
                "type": "text/plain",
                "charset": "utf-8",
                "disposition": "inline",
                "size": text.len()
            })
        }
        (None, Some(html)) => {
            body_values.insert("html".to_string(), json!({ "value": html }));
            json!({
                "partId": "html",
                "type": "text/html",
                "charset": "utf-8",
                "disposition": "inline",
                "size": html.len()
            })
        }
        (None, None) => json!({
            "partId": "text",
            "type": "text/plain",
            "charset": "utf-8",
            "disposition": "inline",
            "size": 0
        }),
    };
    if uploaded.is_empty() {
        return (body_part, Value::Object(body_values));
    }
    let mut sub_parts = vec![body_part];
    for (index, (file, upload)) in draft.files.iter().zip(uploaded.iter()).enumerate() {
        sub_parts.push(json!({
            "partId": format!("attachment{}", index + 1),
            "blobId": upload.blob_id.clone(),
            "type": upload.mime_type.clone(),
            "size": upload.size,
            "name": file.filename.clone(),
            "disposition": "attachment"
        }));
    }
    (
        json!({
            "type": "multipart/mixed",
            "subParts": sub_parts
        }),
        Value::Object(body_values),
    )
}

fn resolve_pull_messages(args: &PullArgs) -> Result<(String, Vec<MessageDetail>)> {
    let include_body = args.include_body && !args.no_body;
    if let Some(message_id) = &args.message_id {
        return Ok((
            format!("message:{message_id}"),
            vec![read_account(args.account, message_id, include_body)?],
        ));
    }
    if let Some(thread_id) = &args.thread_id {
        let thread = thread_account(args.account, thread_id, args.scan_limit, include_body)?;
        return Ok((format!("thread:{thread_id}"), thread.messages));
    }
    let query = args.query.as_ref().expect("selector checked");
    let candidates = search_account(args.account, query, args.limit)?;
    if candidates.is_empty() {
        bail!("no messages matched query {query:?}");
    }
    if args.account == AccountKind::Fastmail {
        let messages = candidates
            .iter()
            .map(|candidate| {
                read_fastmail(&candidate.message_id, args.include_body && !args.no_body)
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok((format!("query:{query}"), messages));
    }
    let unique_threads = candidates
        .iter()
        .filter_map(|message| message.thread_id.as_deref())
        .collect::<Vec<_>>();
    if unique_threads.windows(2).all(|pair| pair[0] == pair[1]) {
        let thread_id = unique_threads[0];
        return Ok((
            format!("query:{query} -> thread:{thread_id}"),
            thread_gmail(args.account, thread_id, args.include_body && !args.no_body)?.messages,
        ));
    }
    if args.select_first {
        let first = &candidates[0];
        return Ok((
            format!("query:{query} -> first-message:{}", first.message_id),
            vec![read_account(args.account, &first.message_id, include_body)?],
        ));
    }
    bail!(
        "ambiguous query {query:?}: {} candidates across multiple threads. Re-run search or use --select-first.",
        candidates.len()
    )
}

fn search_gmail(
    account_kind: AccountKind,
    query: &str,
    limit: usize,
) -> Result<Vec<MessageSummary>> {
    let acct = account(account_kind);
    let refs = list_gmail_messages(Path::new(&acct.config), "me", query, limit)?;
    refs.into_iter()
        .map(|message_ref| {
            let message = get_gmail_message_full(Path::new(&acct.config), "me", &message_ref.id)?;
            Ok(summarize_gmail_message(account_kind, &message))
        })
        .collect()
}

fn read_gmail(
    account_kind: AccountKind,
    message_id: &str,
    include_body: bool,
) -> Result<MessageDetail> {
    let acct = account(account_kind);
    let full = get_gmail_message_full(Path::new(&acct.config), "me", message_id)?;
    let read = if include_body {
        Some(read_gmail_message(Path::new(&acct.config), message_id)?)
    } else {
        None
    };
    let summary = summarize_gmail_message(account_kind, &full);
    let attachments = full
        .payload
        .as_ref()
        .map(collect_gmail_attachment_metas)
        .unwrap_or_default();
    Ok(MessageDetail {
        summary,
        body_text: read.as_ref().and_then(|message| message.body_text.clone()),
        body_html: read.and_then(|message| message.body_html),
        attachments,
    })
}

fn thread_gmail(
    account_kind: AccountKind,
    thread_id: &str,
    include_body: bool,
) -> Result<ThreadDetail> {
    let acct = account(account_kind);
    let thread = get_gmail_thread_full(Path::new(&acct.config), "me", thread_id)?;
    let messages = thread
        .messages
        .unwrap_or_default()
        .into_iter()
        .map(|message| read_gmail(account_kind, &message.id, include_body))
        .collect::<Result<Vec<_>>>()?;
    Ok(ThreadDetail {
        account: account_kind,
        backend: acct.backend.to_string(),
        thread_id: thread_id.to_string(),
        messages,
    })
}

fn list_gmail_messages(
    config_dir: &Path,
    user_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<GwsMessageRef>> {
    let params = json!({
        "userId": user_id,
        "q": query,
        "maxResults": limit,
    });
    let raw = run_gws(
        config_dir,
        [
            "gmail",
            "users",
            "messages",
            "list",
            "--params",
            &params.to_string(),
        ],
    )?;
    let response: GwsMessageListResponse = parse_json_output(&raw)?;
    Ok(response.messages.unwrap_or_default())
}

fn get_gmail_message_full(
    config_dir: &Path,
    user_id: &str,
    message_id: &str,
) -> Result<GwsFullMessage> {
    let params = json!({
        "userId": user_id,
        "id": message_id,
        "format": "full",
    });
    let raw = run_gws(
        config_dir,
        [
            "gmail",
            "users",
            "messages",
            "get",
            "--params",
            &params.to_string(),
        ],
    )?;
    parse_json_output(&raw)
}

fn get_gmail_thread_full(
    config_dir: &Path,
    user_id: &str,
    thread_id: &str,
) -> Result<GwsThreadResponse> {
    let params = json!({
        "userId": user_id,
        "id": thread_id,
        "format": "full",
    });
    let raw = run_gws(
        config_dir,
        [
            "gmail",
            "users",
            "threads",
            "get",
            "--params",
            &params.to_string(),
        ],
    )?;
    parse_json_output(&raw)
}

fn read_gmail_message(config_dir: &Path, message_id: &str) -> Result<GwsReadMessage> {
    let raw = run_gws(
        config_dir,
        [
            "gmail",
            "+read",
            "--id",
            message_id,
            "--headers",
            "--format",
            "json",
        ],
    )?;
    parse_json_output(&raw)
}

fn get_gmail_attachment(
    config_dir: &Path,
    user_id: &str,
    message_id: &str,
    attachment_id: &str,
) -> Result<GwsAttachmentResponse> {
    let params = json!({
        "userId": user_id,
        "messageId": message_id,
        "id": attachment_id,
    });
    let raw = run_gws(
        config_dir,
        [
            "gmail",
            "users",
            "messages",
            "attachments",
            "get",
            "--params",
            &params.to_string(),
        ],
    )?;
    parse_json_output(&raw)
}

fn run_gws<I, S>(config_dir: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let output = Command::new("gws")
        .env("GOOGLE_WORKSPACE_CLI_CONFIG_DIR", config_dir)
        .args(args.into_iter().map(Into::into))
        .output()
        .with_context(|| "run gws")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        bail!("gws failed: {detail}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn summarize_gmail_message(account_kind: AccountKind, message: &GwsFullMessage) -> MessageSummary {
    let payload = message.payload.as_ref();
    MessageSummary {
        account: account_kind,
        backend: "gmail-gws".to_string(),
        message_id: message.id.clone(),
        thread_id: Some(message.thread_id.clone()),
        subject: payload.and_then(|payload| header_value(payload, "Subject")),
        from: payload.and_then(|payload| header_value(payload, "From")),
        to: payload
            .and_then(|payload| header_value(payload, "To"))
            .map(|value| vec![value])
            .unwrap_or_default(),
        date: payload.and_then(|payload| header_value(payload, "Date")),
        snippet: message.snippet.clone(),
        attachment_names: payload.map(attachment_filename_list).unwrap_or_default(),
    }
}

fn collect_gmail_attachment_metas(payload: &GwsPayload) -> Vec<AttachmentMeta> {
    let mut parts = Vec::new();
    collect_gmail_attachment_parts(payload, &mut parts);
    parts
        .into_iter()
        .map(|part| AttachmentMeta {
            filename: part.filename,
            mime_type: Some(part.mime_type),
            size_bytes: None,
            attachment_id: Some(part.attachment_id),
            blob_id: None,
            output_path: None,
            sha256: None,
        })
        .collect()
}

fn collect_gmail_attachment_parts(payload: &GwsPayload, out: &mut Vec<GwsAttachmentPart>) {
    let filename = payload.filename.clone().unwrap_or_default();
    if let Some(attachment_id) = payload
        .body
        .as_ref()
        .and_then(|body| body.attachment_id.clone())
        && !filename.is_empty()
    {
        out.push(GwsAttachmentPart {
            filename,
            mime_type: payload
                .mime_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            attachment_id,
        });
    }
    if let Some(parts) = &payload.parts {
        for part in parts {
            collect_gmail_attachment_parts(part, out);
        }
    }
}

fn header_value(payload: &GwsPayload, name: &str) -> Option<String> {
    payload
        .headers
        .as_ref()?
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.clone())
}

fn attachment_filename_list(payload: &GwsPayload) -> Vec<String> {
    let mut parts = Vec::new();
    collect_gmail_attachment_parts(payload, &mut parts);
    parts.into_iter().map(|part| part.filename).collect()
}

fn search_fastmail(query: &str, limit: usize) -> Result<Vec<MessageSummary>> {
    let acct = account(AccountKind::Fastmail);
    let client = FastmailClient::new(Path::new(&acct.config))?;
    let ids = client.email_query(Some(query), limit)?;
    client.email_get(&ids, false).map(|messages| {
        messages
            .into_iter()
            .map(summarize_fastmail_message)
            .collect::<Vec<_>>()
    })
}

fn read_fastmail(message_id: &str, include_body: bool) -> Result<MessageDetail> {
    let acct = account(AccountKind::Fastmail);
    let client = FastmailClient::new(Path::new(&acct.config))?;
    let mut messages = client.email_get(&[message_id.to_string()], include_body)?;
    let message = messages
        .pop()
        .ok_or_else(|| anyhow!("Fastmail message not found: {message_id}"))?;
    Ok(detail_fastmail_message(message))
}

fn thread_fastmail(thread_id: &str, scan_limit: usize, include_body: bool) -> Result<ThreadDetail> {
    let acct = account(AccountKind::Fastmail);
    let client = FastmailClient::new(Path::new(&acct.config))?;
    let ids = client.thread_email_ids(thread_id).or_else(|_| {
        let ids = client.email_query(None, scan_limit)?;
        let messages = client.email_get(&ids, false)?;
        Ok::<Vec<String>, anyhow::Error>(
            messages
                .into_iter()
                .filter(|message| message.thread_id.as_deref() == Some(thread_id))
                .map(|message| message.id)
                .collect::<Vec<_>>(),
        )
    })?;
    let messages = client.email_get(&ids, include_body)?;
    let details = messages
        .into_iter()
        .filter(|message| message.thread_id.as_deref() == Some(thread_id))
        .map(detail_fastmail_message)
        .collect::<Vec<_>>();
    if details.is_empty() {
        bail!(
            "no Fastmail messages with thread id {thread_id:?} found in recent {scan_limit} messages"
        );
    }
    Ok(ThreadDetail {
        account: AccountKind::Fastmail,
        backend: "fastmail-jmap".to_string(),
        thread_id: thread_id.to_string(),
        messages: details,
    })
}

fn summarize_fastmail_message(message: FastmailEmail) -> MessageSummary {
    MessageSummary {
        account: AccountKind::Fastmail,
        backend: "fastmail-jmap".to_string(),
        message_id: message.id,
        thread_id: message.thread_id,
        subject: message.subject,
        from: message
            .from
            .and_then(|mut from| from.pop())
            .map(format_emailer),
        to: message
            .to
            .unwrap_or_default()
            .into_iter()
            .map(format_emailer)
            .collect(),
        date: message.received_at,
        snippet: message.preview,
        attachment_names: message
            .attachments
            .unwrap_or_default()
            .into_iter()
            .filter_map(|attachment| attachment.name)
            .collect(),
    }
}

fn detail_fastmail_message(message: FastmailEmail) -> MessageDetail {
    let summary = summarize_fastmail_message(message.clone());
    let body_text = message
        .text_body
        .as_ref()
        .and_then(|parts| parts.first())
        .and_then(|part| part.part_id.as_ref())
        .and_then(|part_id| message.body_values.as_ref()?.get(part_id))
        .and_then(|body| body.value.clone());
    let body_html = message
        .html_body
        .as_ref()
        .and_then(|parts| parts.first())
        .and_then(|part| part.part_id.as_ref())
        .and_then(|part_id| message.body_values.as_ref()?.get(part_id))
        .and_then(|body| body.value.clone());
    let attachments = message
        .attachments
        .unwrap_or_default()
        .into_iter()
        .map(|attachment| AttachmentMeta {
            filename: attachment
                .name
                .unwrap_or_else(|| "attachment.bin".to_string()),
            mime_type: attachment.mime_type,
            size_bytes: attachment.size,
            attachment_id: None,
            blob_id: attachment.blob_id,
            output_path: None,
            sha256: None,
        })
        .collect();
    MessageDetail {
        summary,
        body_text,
        body_html,
        attachments,
    }
}

fn format_emailer(emailer: FastmailEmailer) -> String {
    match (emailer.name, emailer.email) {
        (Some(name), Some(email)) if !name.is_empty() => format!("{name} <{email}>"),
        (_, Some(email)) => email,
        (Some(name), _) => name,
        _ => String::new(),
    }
}

fn materialize_attachments(
    account_kind: AccountKind,
    message_id: &str,
    message_index: usize,
    attachments: &[AttachmentMeta],
    attachments_dir: &Path,
) -> Result<Vec<AttachmentMeta>> {
    match account_kind {
        AccountKind::Personal | AccountKind::Domain => materialize_gmail_attachments(
            account_kind,
            message_id,
            message_index,
            attachments,
            attachments_dir,
        ),
        AccountKind::Fastmail => {
            materialize_fastmail_attachments(message_index, attachments, attachments_dir)
        }
    }
}

fn materialize_gmail_attachments(
    account_kind: AccountKind,
    message_id: &str,
    message_index: usize,
    attachments: &[AttachmentMeta],
    attachments_dir: &Path,
) -> Result<Vec<AttachmentMeta>> {
    let acct = account(account_kind);
    attachments
        .iter()
        .enumerate()
        .map(|(attachment_index, attachment)| {
            let attachment_id = attachment
                .attachment_id
                .as_ref()
                .ok_or_else(|| anyhow!("Gmail attachment missing attachment id"))?;
            let raw =
                get_gmail_attachment(Path::new(&acct.config), "me", message_id, attachment_id)?;
            let bytes = decode_gmail_attachment_data(&raw.data)?;
            let output_path = attachment_output_path(
                attachments_dir,
                message_index,
                attachment_index,
                &attachment.filename,
            );
            fs::write(&output_path, &bytes)
                .with_context(|| format!("write {}", output_path.display()))?;
            let mut out = attachment.clone();
            out.size_bytes = Some(bytes.len() as u64);
            out.output_path = Some(output_path.display().to_string());
            out.sha256 = Some(sha256_bytes(&bytes));
            Ok(out)
        })
        .collect()
}

fn materialize_fastmail_attachments(
    message_index: usize,
    attachments: &[AttachmentMeta],
    attachments_dir: &Path,
) -> Result<Vec<AttachmentMeta>> {
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    let acct = account(AccountKind::Fastmail);
    let client = FastmailClient::new(Path::new(&acct.config))?;
    attachments
        .iter()
        .enumerate()
        .map(|(attachment_index, attachment)| {
            let blob_id = attachment
                .blob_id
                .as_ref()
                .ok_or_else(|| anyhow!("Fastmail attachment missing blob id"))?;
            let bytes = client.download_blob(blob_id, &attachment.filename)?;
            let output_path = attachment_output_path(
                attachments_dir,
                message_index,
                attachment_index,
                &attachment.filename,
            );
            fs::write(&output_path, &bytes)
                .with_context(|| format!("write {}", output_path.display()))?;
            let mut out = attachment.clone();
            out.size_bytes = Some(bytes.len() as u64);
            out.output_path = Some(output_path.display().to_string());
            out.sha256 = Some(sha256_bytes(&bytes));
            Ok(out)
        })
        .collect()
}

fn attachment_output_path(
    dir: &Path,
    message_index: usize,
    attachment_index: usize,
    filename: &str,
) -> PathBuf {
    unique_output_path(
        dir,
        &format!(
            "message-{:03}-attachment-{:02}-{}",
            message_index + 1,
            attachment_index + 1,
            sanitize_filename(filename)
        ),
    )
}

#[derive(Debug)]
struct FastmailClient {
    token: String,
    http: Client,
}

impl FastmailClient {
    fn new(env_path: &Path) -> Result<Self> {
        let env = parse_env_file(env_path)?;
        let token = env
            .get("FASTMAIL_API_TOKEN")
            .cloned()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| anyhow!("FASTMAIL_API_TOKEN missing from {}", env_path.display()))?;
        Ok(Self {
            token,
            http: Client::new(),
        })
    }

    fn session(&self) -> Result<FastmailSession> {
        let response = self
            .http
            .get(FASTMAIL_SESSION_URL)
            .bearer_auth(&self.token)
            .send()
            .with_context(|| "fetch Fastmail JMAP session")?
            .error_for_status()
            .with_context(|| "Fastmail JMAP session failed")?;
        response
            .json()
            .with_context(|| "parse Fastmail JMAP session")
    }

    fn email_query(&self, text: Option<&str>, limit: usize) -> Result<Vec<String>> {
        let session = self.session()?;
        let account_id = session.mail_account_id()?;
        let filter = text
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(|query| json!({ "text": query }));
        let mut args = json!({
            "accountId": account_id,
            "limit": limit,
            "sort": [{ "property": "receivedAt", "isAscending": false }]
        });
        if let Some(filter) = filter {
            args["filter"] = filter;
        }
        let response = self.api_call(
            &session.api_url,
            json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                "methodCalls": [["Email/query", args, "q"]]
            }),
        )?;
        let ids = response
            .method_response("Email/query")?
            .get("ids")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Email/query response missing ids"))?
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect();
        Ok(ids)
    }

    fn email_get(&self, ids: &[String], include_body: bool) -> Result<Vec<FastmailEmail>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let session = self.session()?;
        let account_id = session.mail_account_id()?;
        let response = self.api_call(&session.api_url, json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [[
                "Email/get",
                {
                    "accountId": account_id,
                    "ids": ids,
                    "properties": [
                        "id", "threadId", "mailboxIds", "keywords", "receivedAt",
                        "from", "to", "cc", "bcc", "subject", "preview",
                        "textBody", "htmlBody", "bodyValues", "attachments"
                    ],
                    "bodyProperties": ["partId", "blobId", "size", "name", "type", "charset", "disposition", "cid"],
                    "fetchTextBodyValues": include_body,
                    "fetchHTMLBodyValues": include_body,
                    "fetchAllBodyValues": false,
                    "maxBodyValueBytes": 2000000
                },
                "g"
            ]]
        }))?;
        let value = response.method_response("Email/get")?;
        let list = value
            .get("list")
            .cloned()
            .ok_or_else(|| anyhow!("Email/get response missing list"))?;
        serde_json::from_value(list).with_context(|| "parse Email/get list")
    }

    fn thread_email_ids(&self, thread_id: &str) -> Result<Vec<String>> {
        let session = self.session()?;
        let account_id = session.mail_account_id()?;
        let response = self.api_call(
            &session.api_url,
            json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                "methodCalls": [[
                    "Thread/get",
                    {
                        "accountId": account_id,
                        "ids": [thread_id],
                        "properties": ["id", "emailIds"]
                    },
                    "t"
                ]]
            }),
        )?;
        let value = response.method_response("Thread/get")?;
        let list = value
            .get("list")
            .cloned()
            .ok_or_else(|| anyhow!("Thread/get response missing list"))?;
        let mut threads: Vec<FastmailThread> =
            serde_json::from_value(list).with_context(|| "parse Thread/get list")?;
        let thread = threads
            .pop()
            .ok_or_else(|| anyhow!("Fastmail thread not found: {thread_id}"))?;
        Ok(thread.email_ids)
    }

    fn download_blob(&self, blob_id: &str, filename: &str) -> Result<Vec<u8>> {
        let session = self.session()?;
        let account_id = session.mail_account_id()?;
        let url = session
            .download_url
            .replace("{accountId}", &account_id)
            .replace("{blobId}", blob_id)
            .replace("{name}", &sanitize_filename(filename))
            .replace("{type}", "application/octet-stream");
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .with_context(|| "download Fastmail blob")?
            .error_for_status()
            .with_context(|| "Fastmail blob download failed")?;
        Ok(response.bytes()?.to_vec())
    }

    fn create_draft(&self, draft: &PreparedDraft) -> Result<DraftProviderResult> {
        let session = self.session()?;
        let account_id = session.mail_account_id()?;
        let drafts_mailbox_id = self.drafts_mailbox_id(&session, &account_id)?;
        let uploaded = draft
            .files
            .iter()
            .map(|file| self.upload_blob(&session, &account_id, file))
            .collect::<Result<Vec<_>>>()?;
        let (body_structure, body_values) = jmap_body_structure(draft, &uploaded);
        let from = parse_email_address(&draft.from)?;
        let mut mailbox_ids = serde_json::Map::new();
        mailbox_ids.insert(drafts_mailbox_id, json!(true));
        let mut email = json!({
            "mailboxIds": Value::Object(mailbox_ids),
            "keywords": { "$draft": true },
            "from": [{ "name": from.name, "email": from.email }],
            "to": jmap_email_addresses(&draft.to)?,
            "subject": draft.subject.clone(),
            "bodyStructure": body_structure,
            "bodyValues": body_values
        });
        if !draft.cc.is_empty() {
            email["cc"] = json!(jmap_email_addresses(&draft.cc)?);
        }
        if !draft.bcc.is_empty() {
            email["bcc"] = json!(jmap_email_addresses(&draft.bcc)?);
        }
        if !draft.reply_to.is_empty() {
            email["replyTo"] = json!(jmap_email_addresses(&draft.reply_to)?);
        }
        let response = self.api_call(
            &session.api_url,
            json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                "methodCalls": [[
                    "Email/set",
                    {
                        "accountId": account_id,
                        "create": {
                            "draft": email
                        }
                    },
                    "d"
                ]]
            }),
        )?;
        let value = response.method_response("Email/set")?;
        if let Some(not_created) = value.get("notCreated").filter(|value| !value.is_null()) {
            bail!("Fastmail draft creation rejected: {not_created}");
        }
        let created = value
            .get("created")
            .and_then(|created| created.get("draft"))
            .cloned()
            .ok_or_else(|| anyhow!("Email/set response missing created draft"))?;
        let created: FastmailSetCreated =
            serde_json::from_value(created).with_context(|| "parse Email/set created draft")?;
        Ok(DraftProviderResult {
            draft_id: created.id.clone(),
            message_id: Some(created.id),
            thread_id: created.thread_id,
        })
    }

    fn drafts_mailbox_id(&self, session: &FastmailSession, account_id: &str) -> Result<String> {
        let response = self.api_call(
            &session.api_url,
            json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                "methodCalls": [[
                    "Mailbox/get",
                    {
                        "accountId": account_id,
                        "properties": ["id", "name", "role"]
                    },
                    "m"
                ]]
            }),
        )?;
        let value = response.method_response("Mailbox/get")?;
        let list = value
            .get("list")
            .cloned()
            .ok_or_else(|| anyhow!("Mailbox/get response missing list"))?;
        let mailboxes: Vec<FastmailMailbox> =
            serde_json::from_value(list).with_context(|| "parse Mailbox/get list")?;
        mailboxes
            .iter()
            .find(|mailbox| mailbox.role.as_deref() == Some("drafts"))
            .or_else(|| {
                mailboxes
                    .iter()
                    .find(|mailbox| mailbox.name.eq_ignore_ascii_case("Drafts"))
            })
            .map(|mailbox| mailbox.id.clone())
            .ok_or_else(|| anyhow!("Fastmail Drafts mailbox not found"))
    }

    fn upload_blob(
        &self,
        session: &FastmailSession,
        account_id: &str,
        file: &DraftFile,
    ) -> Result<FastmailUploadResponse> {
        let url = session.upload_url.replace("{accountId}", account_id);
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header("Content-Type", &file.mime_type)
            .body(file.bytes.clone())
            .send()
            .with_context(|| format!("upload Fastmail blob {}", file.filename))?
            .error_for_status()
            .with_context(|| format!("Fastmail blob upload failed for {}", file.filename))?;
        response
            .json()
            .with_context(|| format!("parse Fastmail blob upload response for {}", file.filename))
    }

    fn api_call(&self, api_url: &str, body: Value) -> Result<JmapResponse> {
        let response = self
            .http
            .post(api_url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .with_context(|| "call Fastmail JMAP API")?
            .error_for_status()
            .with_context(|| "Fastmail JMAP API failed")?;
        response
            .json()
            .with_context(|| "parse Fastmail JMAP response")
    }
}

fn parse_json_output<T: DeserializeOwned>(raw: &str) -> Result<T> {
    let trimmed = raw.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }
    for line in trimmed.lines().rev() {
        let line = line.trim();
        if (line.starts_with('{') || line.starts_with('['))
            && let Ok(value) = serde_json::from_str(line)
        {
            return Ok(value);
        }
    }
    bail!("could not parse JSON output")
}

fn parse_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut env = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let (key, value) = line.split_once('=').expect("contains equals");
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        env.insert(key.trim().to_string(), value);
    }
    Ok(env)
}

fn decode_gmail_attachment_data(data: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(data))
        .map_err(|error| anyhow!("base64 decode failed: {error}"))
}

fn sanitize_filename(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.contains("-.") {
        out = out.replace("-.", ".");
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "attachment.bin".to_string()
    } else {
        trimmed
    }
}

fn unique_output_path(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("attachment");
    let extension = path.extension().and_then(|value| value.to_str());
    for index in 2.. {
        let name = if let Some(extension) = extension {
            format!("{stem}-{index}.{extension}")
        } else {
            format!("{stem}-{index}")
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", path.display()))
}

fn open_path(path: &Path) -> Result<()> {
    let output = Command::new("open")
        .arg(path)
        .output()
        .with_context(|| format!("open {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "open failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn write_evidence(
    path: &Path,
    account_kind: AccountKind,
    selection: &str,
    messages: &[MessageDetail],
) -> Result<()> {
    let acct = account(account_kind);
    let mut out = String::new();
    out.push_str("# Mailbridge Evidence\n\n");
    out.push_str(&format!("- Account: {} ({:?})\n", acct.email, account_kind));
    out.push_str(&format!("- Backend: {}\n", acct.backend));
    out.push_str(&format!("- Selection: `{selection}`\n"));
    out.push_str(&format!("- Messages: {}\n\n", messages.len()));
    for (index, message) in messages.iter().enumerate() {
        out.push_str(&format!("## Message {}\n\n", index + 1));
        out.push_str(&format!(
            "- Subject: {}\n",
            message.summary.subject.as_deref().unwrap_or("(no subject)")
        ));
        out.push_str(&format!(
            "- From: {}\n",
            message.summary.from.as_deref().unwrap_or("-")
        ));
        out.push_str(&format!(
            "- Date: {}\n",
            message.summary.date.as_deref().unwrap_or("-")
        ));
        out.push_str(&format!("- Message ID: `{}`\n", message.summary.message_id));
        if let Some(thread_id) = &message.summary.thread_id {
            out.push_str(&format!("- Thread ID: `{thread_id}`\n"));
        }
        if !message.attachments.is_empty() {
            out.push_str("- Attachments:\n");
            for attachment in &message.attachments {
                out.push_str(&format!(
                    "  - `{}` ({})\n",
                    attachment.filename,
                    attachment
                        .output_path
                        .as_deref()
                        .unwrap_or("not downloaded")
                ));
            }
        }
        if let Some(body) = &message.body_text {
            out.push_str("\n```text\n");
            out.push_str(body.trim());
            out.push_str("\n```\n\n");
        } else {
            out.push('\n');
        }
    }
    fs::write(path, out).with_context(|| format!("write {}", path.display()))
}

fn default_run_dir(account: AccountKind) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    PathBuf::from(format!("/tmp/mailbridge-{account:?}-{stamp}").to_lowercase())
}

fn command_available(command: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", shell_quote(command)))
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Debug, Deserialize)]
struct GwsAuthStatus {
    user: Option<String>,
    #[serde(default, alias = "tokenValid")]
    token_valid: Option<bool>,
    scopes: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct GwsMessageListResponse {
    messages: Option<Vec<GwsMessageRef>>,
}

#[derive(Debug, Deserialize)]
struct GwsMessageRef {
    id: String,
}

#[derive(Debug, Deserialize)]
struct GwsThreadResponse {
    messages: Option<Vec<GwsFullMessage>>,
}

#[derive(Debug, Deserialize)]
struct GwsFullMessage {
    id: String,
    #[serde(alias = "threadId")]
    thread_id: String,
    payload: Option<GwsPayload>,
    snippet: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GwsPayload {
    #[serde(default)]
    filename: Option<String>,
    #[serde(default, alias = "mimeType")]
    mime_type: Option<String>,
    #[serde(default)]
    headers: Option<Vec<GwsHeader>>,
    #[serde(default)]
    body: Option<GwsBody>,
    #[serde(default)]
    parts: Option<Vec<GwsPayload>>,
}

#[derive(Debug, Deserialize)]
struct GwsHeader {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct GwsBody {
    #[serde(default, alias = "attachmentId")]
    attachment_id: Option<String>,
}

#[derive(Debug)]
struct GwsAttachmentPart {
    filename: String,
    mime_type: String,
    attachment_id: String,
}

#[derive(Debug, Deserialize)]
struct GwsAttachmentResponse {
    data: String,
}

#[derive(Debug, Deserialize)]
struct GwsReadMessage {
    #[serde(default, alias = "bodyText")]
    body_text: Option<String>,
    #[serde(default, alias = "bodyHtml")]
    body_html: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GwsDraftResponse {
    id: String,
    message: Option<GwsDraftMessage>,
}

#[derive(Debug, Deserialize)]
struct GwsDraftMessage {
    id: Option<String>,
    #[serde(default, alias = "threadId")]
    thread_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FastmailSession {
    #[serde(alias = "apiUrl")]
    api_url: String,
    #[serde(alias = "downloadUrl")]
    download_url: String,
    #[serde(alias = "uploadUrl")]
    upload_url: String,
    #[serde(alias = "primaryAccounts")]
    primary_accounts: BTreeMap<String, String>,
    username: Option<String>,
}

impl FastmailSession {
    fn mail_account_id(&self) -> Result<String> {
        self.primary_accounts
            .get("urn:ietf:params:jmap:mail")
            .cloned()
            .or_else(|| self.primary_accounts.values().next().cloned())
            .ok_or_else(|| anyhow!("Fastmail JMAP session missing mail account id"))
    }
}

#[derive(Debug, Deserialize)]
struct JmapResponse {
    #[serde(alias = "methodResponses")]
    method_responses: Vec<(String, Value, String)>,
}

impl JmapResponse {
    fn method_response(&self, name: &str) -> Result<&Value> {
        let (method, value, _) = self
            .method_responses
            .iter()
            .find(|(method, _, _)| method == name || method == "error")
            .ok_or_else(|| anyhow!("JMAP response missing {name}"))?;
        if method == "error" {
            bail!("JMAP error: {value}");
        }
        Ok(value)
    }
}

#[derive(Debug, Deserialize, Clone)]
struct FastmailEmail {
    id: String,
    #[serde(default, alias = "threadId")]
    thread_id: Option<String>,
    #[serde(default, alias = "receivedAt")]
    received_at: Option<String>,
    from: Option<Vec<FastmailEmailer>>,
    to: Option<Vec<FastmailEmailer>>,
    subject: Option<String>,
    preview: Option<String>,
    #[serde(default, alias = "textBody")]
    text_body: Option<Vec<FastmailBodyPart>>,
    #[serde(default, alias = "htmlBody")]
    html_body: Option<Vec<FastmailBodyPart>>,
    #[serde(default, alias = "bodyValues")]
    body_values: Option<BTreeMap<String, FastmailBodyValue>>,
    attachments: Option<Vec<FastmailBodyPart>>,
}

#[derive(Debug, Deserialize, Clone)]
struct FastmailThread {
    #[serde(alias = "emailIds")]
    email_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FastmailMailbox {
    id: String,
    name: String,
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FastmailUploadResponse {
    #[serde(alias = "blobId")]
    blob_id: String,
    #[serde(alias = "type")]
    mime_type: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct FastmailSetCreated {
    id: String,
    #[serde(default, alias = "threadId")]
    thread_id: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct FastmailEmailer {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct FastmailBodyPart {
    #[serde(default, alias = "partId")]
    part_id: Option<String>,
    #[serde(default, alias = "blobId")]
    blob_id: Option<String>,
    #[serde(default, alias = "type")]
    mime_type: Option<String>,
    name: Option<String>,
    size: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
struct FastmailBodyValue {
    value: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_accounts_to_expected_backends() {
        let personal = configured_account(
            AccountKind::Personal,
            "personal",
            "gmail-gws",
            default_personal_gws_config_dir(),
            None,
        );
        let fastmail = configured_account(
            AccountKind::Fastmail,
            "fastmail",
            "fastmail-jmap",
            default_fastmail_env_path(),
            None,
        );

        assert_eq!(
            personal.config,
            default_personal_gws_config_dir().display().to_string()
        );
        assert_eq!(fastmail.backend, "fastmail-jmap");
    }

    #[test]
    fn account_override_replaces_email_and_config() {
        let account = configured_account(
            AccountKind::Personal,
            "default@example.com",
            "gmail-gws",
            PathBuf::from("/default"),
            Some(AccountOverride {
                email: Some("override@example.com".to_string()),
                config: Some("/override".to_string()),
            }),
        );

        assert_eq!(account.email, "override@example.com");
        assert_eq!(account.config, "/override");
        assert_eq!(account.backend, "gmail-gws");
    }

    #[test]
    fn decodes_gmail_urlsafe_attachment_data() {
        let bytes = decode_gmail_attachment_data("JVBERi0xLjMK").unwrap();
        assert_eq!(bytes, b"%PDF-1.3\n");
    }

    #[test]
    fn sanitizes_attachment_filename() {
        assert_eq!(
            sanitize_filename("Tap A Word Function / Adjustments?.pdf"),
            "Tap-A-Word-Function-Adjustments.pdf"
        );
        assert_eq!(sanitize_filename(""), "attachment.bin");
    }

    #[test]
    fn parses_env_file_without_leaking_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("mailbridge.env");
        fs::write(
            &env_path,
            "\n# comment\nFASTMAIL_API_TOKEN='secret-token'\nOTHER=value\n",
        )
        .unwrap();
        let parsed = parse_env_file(&env_path).unwrap();
        assert_eq!(parsed.get("FASTMAIL_API_TOKEN").unwrap(), "secret-token");
        assert_eq!(parsed.get("OTHER").unwrap(), "value");
    }

    #[test]
    fn parses_wrapped_json_output() {
        let raw = "Using keyring backend: keyring\n{\"user\":\"person@example.com\",\"token_valid\":true}";
        let status: GwsAuthStatus = parse_json_output(raw).unwrap();
        assert_eq!(status.user.as_deref(), Some("person@example.com"));
        assert_eq!(status.token_valid, Some(true));
    }

    #[test]
    fn parses_jmap_thread_get_response() {
        let raw = r#"{
          "methodResponses": [
            ["Thread/get", {"list": [{"id": "thread-1", "emailIds": ["a", "b"]}]}, "t"]
          ],
          "sessionState": "state"
        }"#;
        let response: JmapResponse = serde_json::from_str(raw).unwrap();
        let list = response
            .method_response("Thread/get")
            .unwrap()
            .get("list")
            .cloned()
            .unwrap();
        let threads: Vec<FastmailThread> = serde_json::from_value(list).unwrap();

        assert_eq!(threads[0].email_ids, vec!["a", "b"]);
    }

    #[test]
    fn extracts_gmail_headers_and_attachments() {
        let payload = GwsPayload {
            filename: None,
            mime_type: Some("multipart/mixed".to_string()),
            headers: Some(vec![GwsHeader {
                name: "Subject".to_string(),
                value: "Hello".to_string(),
            }]),
            body: None,
            parts: Some(vec![GwsPayload {
                filename: Some("file.pdf".to_string()),
                mime_type: Some("application/pdf".to_string()),
                headers: None,
                body: Some(GwsBody {
                    attachment_id: Some("att-1".to_string()),
                }),
                parts: None,
            }]),
        };
        assert_eq!(header_value(&payload, "subject").as_deref(), Some("Hello"));
        assert_eq!(attachment_filename_list(&payload), vec!["file.pdf"]);
    }

    #[test]
    fn draft_command_does_not_expose_send_subcommand() {
        let parsed = Cli::try_parse_from(["mailbridge", "draft", "send"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn rejects_relative_draft_attachment_paths() {
        let error = load_draft_file(Path::new("screenshot.png"), DraftAttachmentKind::Screenshot)
            .unwrap_err();
        assert!(error.to_string().contains("must be absolute"));
    }

    #[test]
    fn builds_mime_draft_with_screenshot_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let screenshot = dir.path().join("screen.png");
        fs::write(&screenshot, b"png-bytes").unwrap();
        let file = load_draft_file(&screenshot, DraftAttachmentKind::Screenshot).unwrap();
        let draft = PreparedDraft {
            account: Account {
                id: AccountKind::Domain,
                email: "me@example.com".to_string(),
                backend: "gmail-gws".to_string(),
                config: "/tmp/gws".to_string(),
            },
            from: "me@example.com".to_string(),
            to: vec!["you@example.com".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            subject: "Screenshot draft".to_string(),
            text: Some("Please see attached.".to_string()),
            html: None,
            files: vec![file],
        };
        let mime = build_mime_message(&draft).unwrap();

        assert!(mime.contains("To: you@example.com"));
        assert!(mime.contains("Subject: Screenshot draft"));
        assert!(mime.contains("Content-Type: multipart/mixed"));
        assert!(mime.contains("Content-Type: image/png; name=\"screen.png\""));
        assert!(mime.contains("Content-Disposition: attachment; filename=\"screen.png\""));
    }

    #[test]
    fn writes_gmail_upload_mime_in_current_dir() {
        let cwd = env::current_dir().unwrap();
        let path = write_gmail_upload_mime("Subject: Test\r\n\r\nBody").unwrap();
        assert!(path.starts_with(cwd));
        assert_eq!(fs::read_to_string(&path).unwrap(), "Subject: Test\r\n\r\nBody");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn builds_jmap_body_structure_with_uploaded_attachment() {
        let draft = PreparedDraft {
            account: Account {
                id: AccountKind::Fastmail,
                email: "me@example.com".to_string(),
                backend: "fastmail-jmap".to_string(),
                config: "/tmp/fastmail.env".to_string(),
            },
            from: "me@example.com".to_string(),
            to: vec!["you@example.com".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            subject: "Fastmail draft".to_string(),
            text: Some("Body".to_string()),
            html: None,
            files: vec![DraftFile {
                kind: DraftAttachmentKind::Attachment,
                source_path: PathBuf::from("/tmp/file.pdf"),
                filename: "file.pdf".to_string(),
                mime_type: "application/pdf".to_string(),
                bytes: b"pdf".to_vec(),
                sha256: sha256_bytes(b"pdf"),
            }],
        };
        let uploaded = vec![FastmailUploadResponse {
            blob_id: "blob-1".to_string(),
            mime_type: "application/pdf".to_string(),
            size: 3,
        }];
        let (structure, values) = jmap_body_structure(&draft, &uploaded);

        assert_eq!(structure["type"], "multipart/mixed");
        assert_eq!(structure["subParts"][1]["blobId"], "blob-1");
        assert_eq!(structure["subParts"][1]["disposition"], "attachment");
        assert_eq!(values["text"]["value"], "Body");
    }
}
