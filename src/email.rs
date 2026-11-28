//! Outbound transactional email over SMTP.
//!
//! Provider-neutral on purpose: the hosted service points this at Amazon SES,
//! self-hosters can use any SMTP relay or leave it unset. With no SMTP
//! configuration the app falls back to password sign-in and sends nothing.

use crate::config::SmtpConfig;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use std::sync::{Arc, Mutex};

pub struct OutgoingEmail {
    pub to: String,
    pub subject: String,
    pub text: String,
}

pub enum Mailer {
    Disabled,
    Smtp {
        transport: AsyncSmtpTransport<Tokio1Executor>,
        from: Mailbox,
    },
    /// Test double: keeps every message in memory instead of sending it.
    #[cfg_attr(not(test), allow(dead_code))]
    Capture(Arc<Mutex<Vec<OutgoingEmail>>>),
}

impl Mailer {
    pub fn from_config(config: Option<&SmtpConfig>) -> anyhow::Result<Self> {
        let Some(smtp) = config else {
            return Ok(Self::Disabled);
        };
        let from: Mailbox = smtp
            .from
            .parse()
            .map_err(|error| anyhow::anyhow!("EMAIL_FROM is not a valid address: {error}"))?;
        // Port 465 is implicit TLS; anything else (587, 25, 2587) upgrades
        // with STARTTLS, which is refused if the server does not offer it.
        let builder = if smtp.port == 465 {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host)?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host)?
        };
        let transport = builder
            .port(smtp.port)
            .credentials(Credentials::new(
                smtp.username.clone(),
                smtp.password.clone(),
            ))
            .timeout(Some(std::time::Duration::from_secs(15)))
            .build();
        Ok(Self::Smtp { transport, from })
    }

    pub fn enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub async fn send(&self, email: OutgoingEmail) -> anyhow::Result<()> {
        match self {
            Self::Disabled => anyhow::bail!("email is not configured"),
            Self::Capture(sent) => {
                sent.lock().expect("capture mailer lock").push(email);
                Ok(())
            }
            Self::Smtp { transport, from } => {
                let to: Mailbox = email
                    .to
                    .parse()
                    .map_err(|error| anyhow::anyhow!("invalid recipient: {error}"))?;
                let message = Message::builder()
                    .from(from.clone())
                    .to(to)
                    .subject(email.subject)
                    .header(ContentType::TEXT_PLAIN)
                    .body(email.text)?;
                transport.send(message).await?;
                Ok(())
            }
        }
    }
}

pub fn household_invite_email(to: &str, link: &str) -> OutgoingEmail {
    OutgoingEmail {
        to: to.to_owned(),
        subject: "Invitation to join a Pet Health Journal household".into(),
        text: format!(
            "You have been invited to share a household's pet records on Pet Health Journal.\n\n             Open this link to review and accept the invitation:\n\n{link}\n\n             The link expires after seven days and can be used once. Opening it             alone will not create an account; you must press Accept.\n\n             If you did not expect this invitation, ignore this email.\n\nPet Health Journal"
        ),
    }
}

pub fn login_code_email(to: &str, code: &str) -> OutgoingEmail {
    OutgoingEmail {
        to: to.to_owned(),
        // The code is in the subject so phone notifications show it without
        // leaving the ChatGPT sign-in window.
        subject: format!("{code} is your Pet Health Journal sign-in code"),
        text: format!(
            "Your Pet Health Journal sign-in code is:\n\n    {code}\n\n\
             Enter it on the sign-in page. It expires in 10 minutes and works once.\n\n\
             If you didn't try to sign in, you can ignore this email. Nobody can \
             get into your account without this code.\n\n\
             Pet Health Journal\nhttps://pethealthjournal.com\n"
        ),
    }
}
