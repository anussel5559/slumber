//! Template rendering implementation

use crate::{
    collection::{ChainId, ChainRequestTrigger, ChainSource, RecipeId},
    http::{ContentType, RequestBuilder, RequestRecord, Response},
    template::{
        error::TriggeredRequestError, parse::TemplateInputChunk, ChainError,
        Prompt, Template, TemplateChunk, TemplateContext, TemplateError,
        TemplateKey, RECURSION_LIMIT,
    },
    util::ResultExt,
};
use async_trait::async_trait;
use chrono::Utc;
use futures::future::join_all;
use std::{
    env,
    path::Path,
    sync::{atomic::Ordering, Arc},
};
use tokio::{fs, process::Command, sync::oneshot};
use tracing::{info, instrument, trace};

/// Outcome of rendering a single chunk. This allows attaching some metadata to
/// the render.
#[derive(Debug)]
struct RenderedChunk {
    value: String,
    sensitive: bool,
}

type TemplateResult = Result<RenderedChunk, TemplateError>;

impl Template {
    /// Render the template string using values from the given context. If an
    /// error occurs, it is returned as general `anyhow` error. If you need a
    /// more specific error, use [Self::render_stitched].
    pub async fn render(
        &self,
        context: &TemplateContext,
    ) -> anyhow::Result<String> {
        self.render_stitched(context)
            .await
            .map_err(anyhow::Error::from)
            .traced()
    }

    /// Render an optional template. This is useful because `Option::map`
    /// doesn't work with an async operation in the closure
    pub async fn render_opt(
        template: &Option<Self>,
        context: &TemplateContext,
    ) -> anyhow::Result<Option<String>> {
        if let Some(template) = template {
            Ok(Some(template.render(context).await?))
        } else {
            Ok(None)
        }
    }

    /// Render the template string using values from the given context,
    /// returning the individual rendered chunks. This is useful in any
    /// application where rendered chunks need to be handled differently from
    /// raw chunks, e.g. in render previews.
    #[instrument(skip_all, fields(template = self.template))]
    pub async fn render_chunks(
        &self,
        context: &TemplateContext,
    ) -> Vec<TemplateChunk> {
        // Map over each parsed chunk, and render the keys into strings. The
        // raw text chunks will be mapped 1:1
        let futures = self.chunks.iter().copied().map(|chunk| async move {
            match chunk {
                TemplateInputChunk::Raw(span) => TemplateChunk::Raw(span),
                TemplateInputChunk::Key(key) => {
                    // Grab the string corresponding to the span
                    let key = key.map(|span| self.substring(span));

                    // The formatted key should match the source that it was
                    // parsed from, therefore we can use it to match the
                    // override key
                    let raw = key.to_string();
                    // If the key is in the overrides, use the given value
                    // without parsing it
                    let result = match context.overrides.get(&raw) {
                        Some(value) => {
                            trace!(
                                key = raw,
                                value,
                                "Rendered template key from override"
                            );
                            Ok(RenderedChunk {
                                value: value.clone(),
                                // The overriden value *could* be marked
                                // sensitive, but we're taking a shortcut and
                                // assuming it isn't
                                sensitive: false,
                            })
                        }
                        None => {
                            // Standard case - parse the key and render it
                            let result =
                                key.into_source().render(context).await;
                            if let Ok(value) = &result {
                                trace!(
                                    key = raw,
                                    ?value,
                                    "Rendered template key"
                                );
                            }
                            result
                        }
                    };
                    result.into()
                }
            }
        });

        // Parallelization!
        join_all(futures).await
    }

    /// Helper for stitching chunks together into a single string. If any chunk
    /// failed to render, return an error.
    pub(super) async fn render_stitched(
        &self,
        context: &TemplateContext,
    ) -> Result<String, TemplateError> {
        if context.recursion_count.load(Ordering::Relaxed) >= RECURSION_LIMIT {
            return Err(TemplateError::RecursionLimit);
        }

        // Render each individual template chunk in the string
        let chunks = self.render_chunks(context).await;

        // Stitch the rendered chunks together into one string
        let mut buffer = String::with_capacity(self.len());
        for chunk in chunks {
            match chunk {
                TemplateChunk::Raw(span) => {
                    buffer.push_str(self.substring(span));
                }
                TemplateChunk::Rendered { value, .. } => {
                    buffer.push_str(&value)
                }
                TemplateChunk::Error(error) => return Err(error),
            }
        }
        Ok(buffer)
    }
}

impl From<TemplateResult> for TemplateChunk {
    fn from(result: TemplateResult) -> Self {
        match result {
            Ok(outcome) => Self::Rendered {
                value: outcome.value,
                sensitive: outcome.sensitive,
            },
            Err(error) => Self::Error(error),
        }
    }
}

impl<'a> TemplateKey<&'a str> {
    /// Convert this key into a renderable value type
    fn into_source(self) -> Box<dyn TemplateSource<'a>> {
        match self {
            Self::Field(field) => Box::new(FieldTemplateSource { field }),
            Self::Chain(chain_id) => Box::new(ChainTemplateSource {
                chain_id: chain_id.into(),
            }),
            Self::Environment(variable) => {
                Box::new(EnvironmentTemplateSource { variable })
            }
        }
    }
}

/// A single-type parsed template key, which can be rendered into a string.
/// This should be one implementation of this for each variant of [TemplateKey].
///
/// By breaking `TemplateKey` apart into multiple types, we can split the
/// render logic easily amongst a bunch of functions. It's not technically
/// necessary, just a code organization thing.
#[async_trait]
trait TemplateSource<'a>: 'a + Send + Sync {
    /// Render this intermediate value into a string. Return a Cow because
    /// sometimes this can be a reference to the template context, but
    /// other times it has to be owned data (e.g. when pulling response data
    /// from the database).
    async fn render(&self, context: &'a TemplateContext) -> TemplateResult;
}

/// A simple field value (e.g. from the profile or an override)
struct FieldTemplateSource<'a> {
    pub field: &'a str,
}

#[async_trait]
impl<'a> TemplateSource<'a> for FieldTemplateSource<'a> {
    async fn render(&self, context: &'a TemplateContext) -> TemplateResult {
        let field = self.field;

        // Get the value from the profile
        let profile_id = context
            .selected_profile
            .as_ref()
            .ok_or_else(|| TemplateError::NoProfileSelected)?;
        // Typically the caller should validate the ID is valid, this is just
        // a backup check
        let profile =
            context.collection.profiles.get(profile_id).ok_or_else(|| {
                TemplateError::ProfileUnknown {
                    profile_id: profile_id.clone(),
                }
            })?;
        let template = profile.data.get(field).ok_or_else(|| {
            TemplateError::FieldUnknown {
                field: field.to_owned(),
            }
        })?;

        // recursion!
        trace!(%field, %template, "Rendering recursive template");
        context.recursion_count.fetch_add(1, Ordering::Relaxed);
        let rendered =
            template.render_stitched(context).await.map_err(|error| {
                TemplateError::Nested {
                    template: template.clone(),
                    error: Box::new(error),
                }
            })?;
        Ok(RenderedChunk {
            value: rendered,
            sensitive: false,
        })
    }
}

/// A chained value from a complex source. Could be an HTTP response, file, etc.
struct ChainTemplateSource<'a> {
    pub chain_id: ChainId<&'a str>,
}

#[async_trait]
impl<'a> TemplateSource<'a> for ChainTemplateSource<'a> {
    async fn render(&self, context: &'a TemplateContext) -> TemplateResult {
        // Any error in here is the chain error subtype
        let result: Result<_, ChainError> = async {
            // Resolve chained value
            let chain =
                context.collection.chains.get(&self.chain_id).ok_or_else(
                    || ChainError::ChainUnknown((&self.chain_id).into()),
                )?;

            // Resolve the value based on the source type. Also resolve its
            // content type. For responses this will come from its header. For
            // anything else, we'll fall back to the content_type field defined
            // by the user.
            //
            // We intentionally throw the content detection error away here,
            // because it isn't that intuitive for users and is hard to plumb
            let (value, content_type) = match &chain.source {
                ChainSource::Request { recipe, trigger } => {
                    let response =
                        self.get_response(context, recipe, *trigger).await?;
                    // Guess content type based on HTTP header
                    let content_type =
                        ContentType::from_response(&response).ok();
                    (response.body.into_bytes(), content_type)
                }
                ChainSource::File { path } => {
                    // Guess content type based on file extension
                    let content_type = ContentType::from_extension(path).ok();
                    (self.render_file(path).await?, content_type)
                }
                ChainSource::Command { command } => {
                    // No way to guess content type on this
                    (self.render_command(command).await?, None)
                }
                ChainSource::Prompt { message } => (
                    self.render_prompt(
                        context,
                        message.as_deref(),
                        chain.sensitive,
                    )
                    .await?
                    .into_bytes(),
                    // No way to guess content type on this
                    None,
                ),
            };
            // If the user provided a content type, prefer that over the
            // detected one
            let content_type = chain.content_type.or(content_type);

            // If a selector path is present, filter down the value
            let value = if let Some(selector) = &chain.selector {
                let content_type =
                    content_type.ok_or(ChainError::UnknownContentType)?;
                // Parse according to detected content type
                let value = content_type
                    .parse_content(&value)
                    .map_err(|err| ChainError::ParseResponse { error: err })?;
                selector.query_to_string(&*value)?
            } else {
                // We just want raw text - decode as UTF-8
                String::from_utf8(value)
                    .map_err(|error| ChainError::InvalidUtf8 { error })?
            };

            Ok(RenderedChunk {
                value,
                sensitive: chain.sensitive,
            })
        }
        .await;

        // Wrap the chain error into a TemplateError
        result.map_err(|error| TemplateError::Chain {
            chain_id: (&self.chain_id).into(),
            error,
        })
    }
}

impl<'a> ChainTemplateSource<'a> {
    /// Get an HTTP response for a recipe. This will either get the most recent
    /// response from history or re-execute the request, depending on trigger
    /// behavior.
    async fn get_response(
        &self,
        context: &'a TemplateContext,
        recipe_id: &RecipeId,
        trigger: ChainRequestTrigger,
    ) -> Result<Response, ChainError> {
        // Get the referenced recipe. We actually only need the whole recipe if
        // we're executing the request, but we want this to error out if the
        // recipe doesn't exist regardless. It's possible the recipe isn't in
        // the collection but still exists in history (if it was deleted).
        // Eagerly checking for it makes that case error out, which is more
        // intuitive than using history for a deleted recipe.
        let recipe = context
            .collection
            .recipes
            .get_recipe(recipe_id)
            .ok_or_else(|| ChainError::RecipeUnknown(recipe_id.clone()))?;

        // Defer loading the most recent record until we know we'll need it
        let get_most_recent =
            || -> Result<Option<RequestRecord>, ChainError> {
                context
                    .database
                    .get_last_request(
                        context.selected_profile.as_ref(),
                        recipe_id,
                    )
                    .map_err(ChainError::Database)
            };
        // Helper to execute the request, if triggered
        let send_request = || async {
            // There are 3 different ways we can generate the request config:
            // 1. Default (enable all query params/headers)
            // 2. Load from UI state for both TUI and CLI
            // 3. Load from UI state for TUI, enable all for CLI
            // These all have their own issues:
            // 1. Triggered request doesn't necessarily match behavior if user
            //  were to execute the request themself
            // 2. CLI behavior is silently controlled by UI state
            // 3. TUI and CLI behavior may not match
            // All 3 options are unintuitive in some way, but 1 is the easiest
            // to implement so I'm going with that for now.
            let recipe_options = Default::default();

            let builder = RequestBuilder::new(recipe.clone(), recipe_options);
            // Shitty try block
            let result = async {
                let request = builder
                    .build(context)
                    .await
                    .map_err(TriggeredRequestError::Build)?;
                context
                    .http_engine
                    .clone()
                    .ok_or(TriggeredRequestError::NotAllowed)?
                    .send(Arc::new(request))
                    .await
                    .map_err(TriggeredRequestError::Send)
            };
            result.await.map_err(|error| ChainError::Trigger {
                recipe_id: recipe.id.clone(),
                error,
            })
        };

        // Grab the most recent request in history, or send a new request
        let record = match trigger {
            ChainRequestTrigger::Never => {
                get_most_recent()?.ok_or(ChainError::NoResponse)?
            }
            ChainRequestTrigger::NoHistory => {
                // If a record is present in history, use that. If not, fetch
                if let Some(record) = get_most_recent()? {
                    record
                } else {
                    send_request().await?
                }
            }
            ChainRequestTrigger::Expire(duration) => match get_most_recent()? {
                Some(record) if record.end_time + duration >= Utc::now() => {
                    record
                }
                _ => send_request().await?,
            },
            ChainRequestTrigger::Always => send_request().await?,
        };

        Ok(record.response)
    }

    /// Render a chained value from a file
    async fn render_file(&self, path: &'a Path) -> Result<Vec<u8>, ChainError> {
        fs::read(path).await.map_err(|error| ChainError::File {
            path: path.to_owned(),
            error,
        })
    }

    /// Render a chained value from an external command
    async fn render_command(
        &self,
        command: &[String],
    ) -> Result<Vec<u8>, ChainError> {
        match command {
            [] => Err(ChainError::CommandMissing),
            [program, args @ ..] => {
                let output =
                    Command::new(program).args(args).output().await.map_err(
                        |error| ChainError::Command {
                            command: command.to_owned(),
                            error,
                        },
                    )?;
                info!(
                    ?command,
                    stdout = %String::from_utf8_lossy(&output.stdout),
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "Executed subcommand"
                );
                Ok(output.stdout)
            }
        }
    }

    /// Render a value by asking the user to provide it
    async fn render_prompt(
        &self,
        context: &'a TemplateContext,
        label: Option<&str>,
        sensitive: bool,
    ) -> Result<String, ChainError> {
        // Use the prompter to ask the user a question, and wait for a response
        // on the prompt channel
        let (tx, rx) = oneshot::channel();
        context.prompter.prompt(Prompt {
            label: label.unwrap_or(&self.chain_id).into(),
            sensitive,
            channel: tx,
        });
        rx.await.map_err(|_| ChainError::PromptNoResponse)
    }
}

/// A value sourced from the process's environment
struct EnvironmentTemplateSource<'a> {
    pub variable: &'a str,
}

#[async_trait]
impl<'a> TemplateSource<'a> for EnvironmentTemplateSource<'a> {
    async fn render(&self, _: &'a TemplateContext) -> TemplateResult {
        let value = env::var(self.variable).map_err(|err| {
            TemplateError::EnvironmentVariable {
                variable: self.variable.to_owned(),
                error: err,
            }
        })?;
        Ok(RenderedChunk {
            value,
            sensitive: false,
        })
    }
}
