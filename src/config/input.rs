use super::*;

use crate::client::{
    init_client, patch_system_message, ChatCompletionsData, Client, ImageUrl, Message,
    MessageContent, MessageContentPart, MessageContentToolCalls, MessageRole, Model,
};
use crate::function::ToolResult;
use crate::utils::{base64_encode, sha256, AbortSignal};

use anyhow::{bail, Context, Result};
use path_absolutize::Absolutize;
use std::{collections::HashMap, fs::File, io::Read, path::Path};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const IMAGE_EXTS: [&str; 5] = ["png", "jpeg", "jpg", "webp", "gif"];
const SUMMARY_MAX_WIDTH: usize = 80;

#[derive(Debug, Clone)]
pub struct Input {
    config: GlobalConfig,
    text: String,
    raw: (String, Vec<String>),
    patched_text: Option<String>,
    last_reply: Option<String>,
    continue_output: Option<String>,
    regenerate: bool,
    medias: Vec<String>,
    data_urls: HashMap<String, String>,
    tool_calls: Option<MessageContentToolCalls>,
    rag_name: Option<String>,
    role: Role,
    with_session: bool,
    with_agent: bool,
}

impl Input {
    pub fn from_str(config: &GlobalConfig, text: &str, role: Option<Role>) -> Self {
        let (role, with_session, with_agent) = resolve_role(&config.read(), role);
        Self {
            config: config.clone(),
            text: text.to_string(),
            raw: (text.to_string(), vec![]),
            patched_text: None,
            last_reply: None,
            continue_output: None,
            regenerate: false,
            medias: Default::default(),
            data_urls: Default::default(),
            tool_calls: None,
            rag_name: None,
            role,
            with_session,
            with_agent,
        }
    }

    pub async fn from_files(
        config: &GlobalConfig,
        raw_text: &str,
        paths: Vec<String>,
        role: Option<Role>,
    ) -> Result<Self> {
        let mut raw_paths = vec![];
        let mut external_cmds = vec![];
        let mut local_paths = vec![];
        let mut remote_urls = vec![];
        let mut last_reply = None;
        let mut with_last_reply = false;
        for path in paths {
            match resolve_local_path(&path) {
                Some(v) => {
                    if v == "%%" {
                        with_last_reply = true;
                        raw_paths.push(v);
                    } else if v.len() > 2 && v.starts_with('`') && v.ends_with('`') {
                        external_cmds.push(v[1..v.len() - 1].to_string());
                        raw_paths.push(v);
                    } else {
                        if let Ok(path) = Path::new(&v).absolutize() {
                            raw_paths.push(path.display().to_string());
                        }
                        local_paths.push(v);
                    }
                }
                None => {
                    raw_paths.push(path.clone());
                    remote_urls.push(path);
                }
            }
        }
        let (files, medias, data_urls) =
            load_documents(config, external_cmds, local_paths, remote_urls)
                .await
                .context("Failed to load files")?;
        let mut texts = vec![];
        if !raw_text.is_empty() {
            texts.push(raw_text.to_string());
        };
        if with_last_reply {
            if let Some(LastMessage { input, output, .. }) = config.read().last_message.as_ref() {
                if !output.is_empty() {
                    last_reply = Some(output.clone())
                } else if let Some(v) = input.last_reply.as_ref() {
                    last_reply = Some(v.clone());
                }
                if let Some(v) = last_reply.clone() {
                    texts.push(format!("\n{v}\n"));
                }
            }
            if last_reply.is_none() && files.is_empty() && medias.is_empty() {
                bail!("No last reply found");
            }
        }
        for (kind, path, contents) in files {
            texts.push(format!(
                "\n============ {kind}: {path} ============\n{contents}"
            ));
        }
        let (role, with_session, with_agent) = resolve_role(&config.read(), role);
        Ok(Self {
            config: config.clone(),
            text: texts.join("\n"),
            raw: (raw_text.to_string(), raw_paths),
            patched_text: None,
            last_reply,
            continue_output: None,
            regenerate: false,
            medias,
            data_urls,
            tool_calls: Default::default(),
            rag_name: None,
            role,
            with_session,
            with_agent,
        })
    }

    pub async fn from_files_with_spinner(
        config: &GlobalConfig,
        raw_text: &str,
        paths: Vec<String>,
        role: Option<Role>,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        abortable_run_with_spinner(
            Input::from_files(config, raw_text, paths, role),
            "Loading files",
            abort_signal,
        )
        .await
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.medias.is_empty()
    }

    pub fn data_urls(&self) -> HashMap<String, String> {
        self.data_urls.clone()
    }

    pub fn tool_calls(&self) -> &Option<MessageContentToolCalls> {
        &self.tool_calls
    }

    pub fn text(&self) -> String {
        match self.patched_text.clone() {
            Some(text) => text,
            None => self.text.clone(),
        }
    }

    pub fn clear_patch(&mut self) {
        self.patched_text = None;
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
    }

    pub fn stream(&self) -> bool {
        self.config.read().stream && !self.role().model().no_stream()
    }

    pub fn continue_output(&self) -> Option<&str> {
        self.continue_output.as_deref()
    }

    pub fn set_continue_output(&mut self, output: &str) {
        let output = match &self.continue_output {
            Some(v) => format!("{v}{output}"),
            None => output.to_string(),
        };
        self.continue_output = Some(output);
    }

    pub fn regenerate(&self) -> bool {
        self.regenerate
    }

    pub fn set_regenerate(&mut self) {
        let role = self.config.read().extract_role();
        if role.name() == self.role().name() {
            self.role = role;
        }
        self.regenerate = true;
    }

    pub async fn use_embeddings(&mut self, abort_signal: AbortSignal) -> Result<()> {
        if self.text.is_empty() {
            return Ok(());
        }
        if !self.text.is_empty() {
            let rag = self.config.read().rag.clone();
            if let Some(rag) = rag {
                let result =
                    Config::search_rag(&self.config, &rag, &self.text, abort_signal).await?;
                self.patched_text = Some(result);
                self.rag_name = Some(rag.name().to_string());
            }
        }
        Ok(())
    }

    pub fn rag_name(&self) -> Option<&str> {
        self.rag_name.as_deref()
    }

    pub fn merge_tool_results(mut self, output: String, tool_results: Vec<ToolResult>) -> Self {
        match self.tool_calls.as_mut() {
            Some(exist_tool_results) => {
                exist_tool_results.merge(tool_results, output);
            }
            None => self.tool_calls = Some(MessageContentToolCalls::new(tool_results, output)),
        }
        self
    }

    pub fn create_client(&self) -> Result<Box<dyn Client>> {
        init_client(&self.config, Some(self.role().model().clone()))
    }

    pub fn prepare_completion_data(
        &self,
        model: &Model,
        stream: bool,
    ) -> Result<ChatCompletionsData> {
        if !self.medias.is_empty() && !model.supports_vision() {
            bail!("The current model does not support vision. Is the model configured with `supports_vision: true`?");
        }
        let mut messages = self.build_messages()?;
        if model.no_system_message() {
            patch_system_message(&mut messages);
        }
        model.guard_max_input_tokens(&messages)?;
        let temperature = self.role().temperature();
        let top_p = self.role().top_p();
        let functions = self.config.read().select_functions(self.role());
        Ok(ChatCompletionsData {
            messages,
            temperature,
            top_p,
            functions,
            stream,
        })
    }

    pub fn build_messages(&self) -> Result<Vec<Message>> {
        let mut messages = if let Some(session) = self.session(&self.config.read().session) {
            session.build_messages(self)
        } else {
            self.role().build_messages(self)
        };
        if let Some(tool_calls) = &self.tool_calls {
            messages.push(Message::new(
                MessageRole::Assistant,
                MessageContent::ToolCalls(tool_calls.clone()),
            ))
        }
        Ok(messages)
    }

    pub fn echo_messages(&self) -> String {
        if let Some(session) = self.session(&self.config.read().session) {
            session.echo_messages(self)
        } else {
            self.role().echo_messages(self)
        }
    }

    pub fn role(&self) -> &Role {
        &self.role
    }

    pub fn session<'a>(&self, session: &'a Option<Session>) -> Option<&'a Session> {
        if self.with_session {
            session.as_ref()
        } else {
            None
        }
    }

    pub fn session_mut<'a>(&self, session: &'a mut Option<Session>) -> Option<&'a mut Session> {
        if self.with_session {
            session.as_mut()
        } else {
            None
        }
    }

    pub fn with_agent(&self) -> bool {
        self.with_agent
    }

    pub fn summary(&self) -> String {
        let text: String = self
            .text
            .trim()
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if text.width_cjk() > SUMMARY_MAX_WIDTH {
            let mut sum_width = 0;
            let mut chars = vec![];
            for c in text.chars() {
                sum_width += c.width_cjk().unwrap_or(1);
                if sum_width > SUMMARY_MAX_WIDTH - 3 {
                    chars.extend(['.', '.', '.']);
                    break;
                }
                chars.push(c);
            }
            chars.into_iter().collect()
        } else {
            text
        }
    }

    pub fn raw(&self) -> String {
        let (text, files) = &self.raw;
        let mut segments = files.to_vec();
        if !segments.is_empty() {
            segments.insert(0, ".file".into());
        }
        if !text.is_empty() {
            if !segments.is_empty() {
                segments.push("--".into());
            }
            segments.push(text.clone());
        }
        segments.join(" ")
    }

    pub fn render(&self) -> String {
        let text = self.text();
        if self.medias.is_empty() {
            return text;
        }
        let tail_text = if text.is_empty() {
            String::new()
        } else {
            format!(" -- {text}")
        };
        let files: Vec<String> = self
            .medias
            .iter()
            .cloned()
            .map(|url| resolve_data_url(&self.data_urls, url))
            .collect();
        format!(".file {}{}", files.join(" "), tail_text)
    }

    pub fn message_content(&self) -> MessageContent {
        if self.medias.is_empty() {
            MessageContent::Text(self.text())
        } else {
            let mut list: Vec<MessageContentPart> = self
                .medias
                .iter()
                .cloned()
                .map(|url| MessageContentPart::ImageUrl {
                    image_url: ImageUrl { url },
                })
                .collect();
            if !self.text.is_empty() {
                list.insert(0, MessageContentPart::Text { text: self.text() });
            }
            MessageContent::Array(list)
        }
    }
}

fn resolve_role(config: &Config, role: Option<Role>) -> (Role, bool, bool) {
    match role {
        Some(v) => (v, false, false),
        None => (
            config.extract_role(),
            config.session.is_some(),
            config.agent.is_some(),
        ),
    }
}

async fn load_documents(
    config: &GlobalConfig,
    external_cmds: Vec<String>,
    local_paths: Vec<String>,
    remote_urls: Vec<String>,
) -> Result<(
    Vec<(&'static str, String, String)>,
    Vec<String>,
    HashMap<String, String>,
)> {
    let mut files = vec![];
    let mut medias = vec![];
    let mut data_urls = HashMap::new();
    for cmd in external_cmds {
        let (success, stdout, stderr) =
            run_command_with_output(&SHELL.cmd, &[&SHELL.arg, &cmd], None)?;
        if !success {
            let err = if !stderr.is_empty() { stderr } else { stdout };
            bail!("Failed to run `{cmd}`\n{err}");
        }
        files.push(("CMD", cmd, stdout));
    }

    let local_files = expand_glob_paths(&local_paths, true).await?;
    let loaders = config.read().document_loaders.clone();
    for file_path in local_files {
        if is_image(&file_path) {
            let contents = read_media_to_data_url(&file_path)
                .with_context(|| format!("Unable to read media file '{file_path}'"))?;
            data_urls.insert(sha256(&contents), file_path);
            medias.push(contents)
        } else {
            let document = load_file(&loaders, &file_path)
                .await
                .with_context(|| format!("Unable to read file '{file_path}'"))?;
            files.push(("FILE", file_path, document.contents));
        }
    }

    for file_url in remote_urls {
        let (contents, extension) = fetch(&loaders, &file_url, true)
            .await
            .with_context(|| format!("Failed to load url '{file_url}'"))?;
        if extension == MEDIA_URL_EXTENSION {
            data_urls.insert(sha256(&contents), file_url);
            medias.push(contents)
        } else {
            files.push(("URL", file_url, contents));
        }
    }
    Ok((files, medias, data_urls))
}

pub fn resolve_data_url(data_urls: &HashMap<String, String>, data_url: String) -> String {
    if data_url.starts_with("data:") {
        let hash = sha256(&data_url);
        if let Some(path) = data_urls.get(&hash) {
            return path.to_string();
        }
        data_url
    } else {
        data_url
    }
}

fn resolve_local_path(path: &str) -> Option<String> {
    if is_url(path) {
        return None;
    }
    let new_path = if let (Some(file), Some(home)) = (path.strip_prefix("~/"), dirs::home_dir()) {
        home.join(file).display().to_string()
    } else {
        path.to_string()
    };
    Some(new_path)
}

fn is_image(path: &str) -> bool {
    get_patch_extension(path)
        .map(|v| IMAGE_EXTS.contains(&v.as_str()))
        .unwrap_or_default()
}

fn read_media_to_data_url(image_path: &str) -> Result<String> {
    let extension = get_patch_extension(image_path).unwrap_or_default();
    let mime_type = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => bail!("Unexpected media type"),
    };
    let mut file = File::open(image_path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    let encoded_image = base64_encode(buffer);
    let data_url = format!("data:{};base64,{}", mime_type, encoded_image);

    Ok(data_url)
}
