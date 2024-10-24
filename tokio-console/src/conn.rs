use console_api::instrument::StateRequest;
use console_api::instrument::{
    instrument_client::InstrumentClient, InstrumentRequest, PauseRequest, ResumeRequest,
    TaskDetailsRequest, Update,
};
use console_api::tasks::TaskDetails;
use futures::stream::StreamExt;
use futures::TryFutureExt;
use hyper_util::rt::TokioIo;
use std::{error::Error, time::Duration};
#[cfg(unix)]
use tokio::net::UnixStream;
use tonic::{
    transport::{Channel, Endpoint, Uri},
    Streaming,
};

#[derive(Debug)]
pub struct Connection {
    target: Uri,
    state: State,
}

// clippy doesn't like that the "connected" case is much larger than the
// disconnected case, and suggests boxing the connected side's stream.
// however, this is rarely disconnected; it's normally connected. boxing the
// stream just adds a heap pointer dereference, slightly penalizing polling
// the stream in most cases. so, don't listen to clippy on this.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum State {
    Connected {
        client: InstrumentClient<Channel>,
        update_stream: Box<Streaming<Update>>,
        state_stream: Box<Streaming<console_api::instrument::State>>,
    },
    Disconnected(Duration),
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum Message {
    Update(Update),
    State(console_api::instrument::State),
}

macro_rules! with_client {
    ($me:ident, $client:ident, $block:expr) => ({
        loop {
            match $me.state {
                State::Connected { client: ref mut $client, .. } => {
                    match $block {
                        Ok(resp) => break Ok(resp),
                        // If the error is a `h2::Error`, that indicates
                        // something went wrong at the connection level, rather
                        // than the server returning an error code. In that
                        // case, let's try reconnecting...
                        Err(error) if error.source().iter().any(|src| src.is::<h2::Error>()) => {
                            tracing::warn!(
                                error = %error,
                                "connection error sending command"
                            );
                            $me.state = State::Disconnected(Self::BACKOFF);
                        }
                        // Otherwise, return the error.
                        Err(e) => {
                            break Err(e);
                        }
                    }
                }
                State::Disconnected(_) => $me.connect().await,
            }
        }
    })
}

impl Connection {
    const BACKOFF: Duration = Duration::from_millis(500);
    pub fn new(target: Uri) -> Self {
        Self {
            target,
            state: State::Disconnected(Duration::from_secs(0)),
        }
    }

    async fn connect(&mut self) {
        const MAX_BACKOFF: Duration = Duration::from_secs(5);

        while let State::Disconnected(backoff) = self.state {
            if backoff == Duration::from_secs(0) {
                tracing::debug!(to = %self.target, "connecting");
            } else {
                tracing::debug!(reconnect_in = ?backoff, "reconnecting");
                tokio::time::sleep(backoff).await;
            }
            let try_connect = async {
                let channel = match self.target.scheme_str() {
                    #[cfg(unix)]
                    Some("file") => {
                        if !matches!(self.target.host(), None | Some("localhost")) {
                            return Err("cannot connect to non-localhost unix domain socket".into());
                        }
                        let path = self.target.path().to_owned();
                        // Dummy endpoint is ignored by the connector.
                        let endpoint = Endpoint::from_static("http://localhost");
                        endpoint
                            .connect_with_connector(tower::service_fn(move |_| {
                                UnixStream::connect(path.clone()).map_ok(TokioIo::new)
                            }))
                            .await?
                    }
                    #[cfg(not(unix))]
                    Some("file") => {
                        return Err("unix domain sockets are not supported on this platform".into());
                    }
                    _ => {
                        let endpoint = Endpoint::from(self.target.clone());
                        endpoint.connect().await?
                    }
                };
                let mut client = InstrumentClient::new(channel);
                let update_request = tonic::Request::new(InstrumentRequest {});
                let update_stream =
                    Box::new(client.watch_updates(update_request).await?.into_inner());
                let state_request = tonic::Request::new(StateRequest {});
                let state_stream = Box::new(client.watch_state(state_request).await?.into_inner());
                Ok::<State, Box<dyn Error + Send + Sync>>(State::Connected {
                    client,
                    update_stream,
                    state_stream,
                })
            };
            self.state = match try_connect.await {
                Ok(connected) => {
                    tracing::debug!("connected successfully!");
                    connected
                }
                Err(error) => {
                    tracing::warn!(%error, "error connecting");
                    let backoff = std::cmp::max(backoff + Self::BACKOFF, MAX_BACKOFF);
                    State::Disconnected(backoff)
                }
            };
        }
    }

    pub async fn next_message(&mut self) -> Message {
        loop {
            match &mut self.state {
                State::Connected {
                    update_stream,
                    state_stream,
                    ..
                } => {
                    tokio::select! {
                        update = update_stream.next() => match update {
                            Some(Ok(update)) => return Message::Update(update),
                            Some(Err(status)) => {
                                tracing::warn!(%status, "error from update stream");
                                self.state = State::Disconnected(Self::BACKOFF);
                            }
                            None => {
                                tracing::error!("update stream closed by server");
                                self.state = State::Disconnected(Self::BACKOFF);
                            }
                        },
                        state = state_stream.next() => match state {
                            Some(Ok(state)) => return Message::State(state),
                            Some(Err(status)) => {
                                tracing::warn!(%status, "error from state stream");
                                self.state = State::Disconnected(Self::BACKOFF);
                            }
                            None => {
                                tracing::error!("state stream closed by server");
                                self.state = State::Disconnected(Self::BACKOFF);
                            }
                        },
                    }
                }
                State::Disconnected(_) => self.connect().await,
            }
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn watch_details(
        &mut self,
        task_id: u64,
    ) -> Result<Streaming<TaskDetails>, tonic::Status> {
        with_client!(self, client, {
            let request = tonic::Request::new(TaskDetailsRequest {
                id: Some(task_id.into()),
            });
            client.watch_task_details(request).await
        })
        .map(|watch| watch.into_inner())
    }

    #[tracing::instrument(skip(self))]
    pub async fn pause(&mut self) {
        let res = with_client!(self, client, {
            let request = tonic::Request::new(PauseRequest {});
            client.pause(request).await
        });

        if let Err(e) = res {
            tracing::error!(error = %e, "rpc error sending pause command");
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn resume(&mut self) {
        let res = with_client!(self, client, {
            let request = tonic::Request::new(ResumeRequest {});
            client.resume(request).await
        });

        if let Err(e) = res {
            tracing::error!(error = %e, "rpc error sending resume command");
        }
    }

    pub fn render(&self, styles: &crate::view::Styles) -> ratatui::text::Line {
        use ratatui::{
            style::{Color, Modifier},
            text::{Line, Span},
        };
        let state = match self.state {
            State::Connected { .. } => Span::styled(
                "(CONNECTED)",
                styles.fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            State::Disconnected(d) if d == Duration::from_secs(0) => Span::styled(
                "(CONNECTING)",
                styles.fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            State::Disconnected(d) => Span::styled(
                format!("(RECONNECTING IN {:?})", d),
                styles.fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
        };
        Line::from(vec![
            Span::raw("connection: "),
            Span::raw(self.target.to_string()),
            Span::raw(" "),
            state,
        ])
    }
}
