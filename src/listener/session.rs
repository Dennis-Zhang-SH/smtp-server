use std::net::IpAddr;

use smtp_proto::{
    request::receiver::{
        BdatReceiver, DataReceiver, DummyDataReceiver, DummyLineReceiver, LineReceiver,
        MAX_LINE_LENGTH,
    },
    *,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    config::ServerProtocol,
    core::{Envelope, Session, State},
};

use super::auth::SaslToken;

impl<T: AsyncWrite + AsyncRead + Unpin> Session<T> {
    pub async fn ingest(&mut self, bytes: &[u8]) -> Result<bool, ()> {
        let mut iter = bytes.iter();
        let mut state = std::mem::replace(&mut self.state, State::None);

        'outer: loop {
            match &mut state {
                State::Request(receiver) => loop {
                    match receiver.ingest(&mut iter, bytes) {
                        Ok(request) => match request {
                            Request::Rcpt { to } => {
                                self.handle_rcpt_to(to).await?;
                            }
                            Request::Mail { from } => {
                                self.handle_mail_from(from).await?;
                            }
                            Request::Ehlo { host } => {
                                if self.instance.protocol == ServerProtocol::Smtp {
                                    self.handle_ehlo(host).await?;
                                } else {
                                    self.write(b"500 5.5.1 Invalid command.\r\n").await?;
                                }
                            }
                            Request::Data => {
                                self.eval_data_params().await;
                                if self.can_send_data().await? {
                                    self.write(b"354 Start mail input; end with <CRLF>.<CRLF>\r\n")
                                        .await?;
                                    self.data.message = Vec::with_capacity(1024);
                                    state = State::Data(DataReceiver::new());
                                    continue 'outer;
                                }
                            }
                            Request::Bdat {
                                chunk_size,
                                is_last,
                            } => {
                                self.eval_data_params().await;
                                state = if chunk_size + self.data.message.len()
                                    < self.params.data_max_message_size
                                {
                                    if self.data.message.is_empty() {
                                        self.data.message = Vec::with_capacity(chunk_size);
                                    } else {
                                        self.data.message.reserve(chunk_size);
                                    }
                                    State::Bdat(BdatReceiver::new(chunk_size, is_last))
                                } else {
                                    // Chunk is too large, ignore.
                                    State::DataTooLarge(DummyDataReceiver::new_bdat(chunk_size))
                                };
                                continue 'outer;
                            }
                            Request::Auth {
                                mechanism,
                                initial_response,
                            } => {
                                // TODO no plain auth over plaintext
                                if self.params.auth == 0 || self.params.auth_lookup.is_none() {
                                    self.write(b"503 5.5.1 AUTH not allowed.\r\n").await?;
                                } else if !self.data.authenticated_as.is_empty() {
                                    self.write(b"503 5.5.1 Already authenticated.\r\n").await?;
                                } else if let Some(mut token) =
                                    SaslToken::from_mechanism(mechanism & self.params.auth)
                                {
                                    if self
                                        .handle_sasl_response(
                                            &mut token,
                                            initial_response.as_bytes(),
                                        )
                                        .await?
                                    {
                                        state = State::Sasl(LineReceiver::new(token));
                                        continue 'outer;
                                    }
                                } else {
                                    self.write(
                                        b"554 5.7.8 Authentication mechanism not supported.\r\n",
                                    )
                                    .await?;
                                }
                            }
                            Request::Noop { .. } => {
                                self.write(b"250 2.0.0 OK\r\n").await?;
                            }
                            Request::Vrfy { value } => {
                                self.handle_vrfy(value).await?;
                            }
                            Request::Expn { value } => {
                                self.handle_expn(value).await?;
                            }
                            Request::StartTls => {
                                if self.params.starttls {
                                    self.write(b"220 2.0.0 Ready to start TLS.\r\n").await?;
                                    self.state = State::default();
                                    return Ok(false);
                                } else {
                                    self.write(b"504 5.7.4 Unable to start TLS.\r\n").await?;
                                }
                            }
                            Request::Rset => {
                                self.reset();
                                self.write(b"250 2.0.0 OK\r\n").await?;
                            }
                            Request::Quit => {
                                self.write(b"221 2.0.0 Bye.\r\n").await?;
                                return Err(());
                            }
                            Request::Help { .. } => {
                                self.write(
                                    b"250 2.0.0 Help can be found at https://stalw.art/smtp/\r\n",
                                )
                                .await?;
                            }
                            Request::Helo { host } => {
                                if self.instance.protocol == ServerProtocol::Smtp
                                    && self.data.helo_domain.is_empty()
                                {
                                    self.data.helo_domain = host;
                                    self.eval_mail_params().await;
                                    self.write(
                                        format!("250 {} says hello\r\n", self.instance.hostname)
                                            .as_bytes(),
                                    )
                                    .await?;
                                } else {
                                    self.write(b"503 5.5.1 Invalid command.\r\n").await?;
                                }
                            }
                            Request::Lhlo { host } => {
                                if self.instance.protocol == ServerProtocol::Lmtp {
                                    self.handle_ehlo(host).await?;
                                } else {
                                    self.write(b"502 5.5.1 Invalid command.\r\n").await?;
                                }
                            }
                            Request::Etrn { .. } | Request::Atrn { .. } | Request::Burl { .. } => {
                                self.write(b"502 5.5.1 Command not implemented.\r\n")
                                    .await?;
                            }
                        },
                        Err(err) => match err {
                            Error::NeedsMoreData { .. } => break 'outer,
                            Error::UnknownCommand | Error::InvalidResponse { .. } => {
                                self.write(b"500 5.5.1 Invalid command.\r\n").await?;
                            }
                            Error::InvalidSenderAddress => {
                                self.write(b"501 5.1.8 Bad sender's system address.\r\n")
                                    .await?;
                            }
                            Error::InvalidRecipientAddress => {
                                self.write(
                                    b"501 5.1.3 Bad destination mailbox address syntax.\r\n",
                                )
                                .await?;
                            }
                            Error::SyntaxError { syntax } => {
                                self.write(
                                    format!("501 5.5.2 Syntax error, expected: {}\r\n", syntax)
                                        .as_bytes(),
                                )
                                .await?;
                            }
                            Error::InvalidParameter { param } => {
                                self.write(
                                    format!("501 5.5.4 Invalid parameter {:?}.\r\n", param)
                                        .as_bytes(),
                                )
                                .await?;
                            }
                            Error::UnsupportedParameter { param } => {
                                self.write(
                                    format!("504 5.5.4 Unsupported parameter {:?}.\r\n", param)
                                        .as_bytes(),
                                )
                                .await?;
                            }
                            Error::ResponseTooLong => {
                                state = State::RequestTooLarge(DummyLineReceiver::default());
                                continue 'outer;
                            }
                        },
                    }
                },
                State::Data(receiver) => {
                    if self.data.message.len() + bytes.len() < self.params.data_max_message_size {
                        if receiver.ingest(&mut iter, &mut self.data.message) {
                            self.queue_message().await?;
                            state = State::default();
                        } else {
                            break 'outer;
                        }
                    } else {
                        state = State::DataTooLarge(DummyDataReceiver::new_data(receiver));
                    }
                }
                State::Bdat(receiver) => {
                    if receiver.ingest(&mut iter, &mut self.data.message) {
                        if self.can_send_data().await? {
                            if receiver.is_last {
                                self.queue_message().await?;
                            } else {
                                self.write(b"250 2.6.0 Chunk accepted.\r\n").await?;
                            }
                        } else {
                            self.data.message = Vec::with_capacity(0);
                        }
                        state = State::default();
                    } else {
                        break 'outer;
                    }
                }
                State::Sasl(receiver) => {
                    if receiver.ingest(&mut iter) {
                        if receiver.buf.len() < MAX_LINE_LENGTH {
                            if self
                                .handle_sasl_response(&mut receiver.state, &receiver.buf)
                                .await?
                            {
                                receiver.buf.clear();
                                continue 'outer;
                            }
                        } else {
                            self.auth_error(
                                b"500 5.5.6 Authentication Exchange line is too long.\r\n",
                            )
                            .await?;
                        }
                        state = State::default();
                    } else {
                        break 'outer;
                    }
                }
                State::DataTooLarge(receiver) => {
                    if receiver.ingest(&mut iter) {
                        self.data.message = Vec::with_capacity(0);
                        self.write(b"552 5.3.4 Message too big for system.\r\n")
                            .await?;
                        state = State::default();
                    } else {
                        break 'outer;
                    }
                }
                State::RequestTooLarge(receiver) => {
                    if receiver.ingest(&mut iter) {
                        self.write(b"554 5.3.4 Line is too long.\r\n").await?;
                        state = State::default();
                    } else {
                        break 'outer;
                    }
                }
                State::None => unreachable!(),
            }
        }
        self.state = state;

        Ok(true)
    }

    pub fn reset(&mut self) {
        self.data.mail_from = None;
        self.data.rcpt_to.clear();
        self.data.message = Vec::with_capacity(0);
        self.data.priority = 0;
    }

    #[inline(always)]
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), ()> {
        match self.stream.write_all(bytes).await {
            Ok(_) => {
                tracing::trace!(parent: &self.span,
                                event = "write",
                                data = std::str::from_utf8(bytes).unwrap_or_default(),
                                size = bytes.len());
                Ok(())
            }
            Err(err) => {
                tracing::debug!(parent: &self.span,
                                event = "error",
                                class = "io",
                                "Failed to write to stream: {:?}", err);
                Err(())
            }
        }
    }

    #[inline(always)]
    pub async fn read(&mut self, bytes: &mut [u8]) -> Result<usize, ()> {
        match self.stream.read(bytes).await {
            Ok(len) => {
                tracing::trace!(parent: &self.span,
                                event = "read",
                                data =  bytes
                                        .get(0..len)
                                        .and_then(|bytes| std::str::from_utf8(bytes).ok())
                                        .unwrap_or_default(),
                                size = len);
                Ok(len)
            }
            Err(err) => {
                tracing::debug!(
                    parent: &self.span,
                    event = "error",
                    class = "io",
                    "Failed to read from stream: {:?}", err
                );
                Err(())
            }
        }
    }
}

impl<T: AsyncRead + AsyncWrite> Envelope for Session<T> {
    #[inline(always)]
    fn local_ip(&self) -> &IpAddr {
        &self.data.local_ip
    }

    #[inline(always)]
    fn remote_ip(&self) -> &IpAddr {
        &self.data.remote_ip
    }

    #[inline(always)]
    fn sender_domain(&self) -> &str {
        self.data
            .mail_from
            .as_ref()
            .map(|a| a.domain.as_str())
            .unwrap_or_default()
    }

    #[inline(always)]
    fn sender(&self) -> &str {
        self.data
            .mail_from
            .as_ref()
            .map(|a| a.address_lcase.as_str())
            .unwrap_or_default()
    }

    #[inline(always)]
    fn rcpt_domain(&self) -> &str {
        self.data
            .rcpt_to
            .last()
            .map(|r| r.domain.as_str())
            .unwrap_or_default()
    }

    #[inline(always)]
    fn rcpt(&self) -> &str {
        self.data
            .rcpt_to
            .last()
            .map(|r| r.address_lcase.as_str())
            .unwrap_or_default()
    }

    #[inline(always)]
    fn helo_domain(&self) -> &str {
        self.data.helo_domain.as_str()
    }

    #[inline(always)]
    fn authenticated_as(&self) -> &str {
        self.data.authenticated_as.as_str()
    }

    #[inline(always)]
    fn mx(&self) -> &str {
        ""
    }

    #[inline(always)]
    fn listener_id(&self) -> u16 {
        self.instance.listener_id
    }

    #[inline(always)]
    fn priority(&self) -> i16 {
        self.data.priority
    }
}
