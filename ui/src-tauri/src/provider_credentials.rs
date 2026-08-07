use anyhow::{Result, bail};
use keyring::{Entry, Error};
use zeroize::Zeroizing;

const SERVICE: &str = "dev.agentdesktop.provider";
const ACCOUNT: &str = "anthropic-api-key";
const ENVIRONMENT_NAME: &str = "ANTHROPIC_API_KEY";

fn entry() -> Result<Entry> {
    Ok(Entry::new(SERVICE, ACCOUNT)?)
}

pub fn environment_is_configured() -> bool {
    std::env::var_os(ENVIRONMENT_NAME).is_some_and(|value| !value.is_empty())
}

pub fn is_configured() -> Result<bool> {
    Ok(load()?.is_some() || environment_is_configured())
}

pub fn load() -> Result<Option<Zeroizing<Vec<u8>>>> {
    match entry()?.get_secret() {
        Ok(secret) if secret.is_empty() => Ok(None),
        Ok(secret) => Ok(Some(Zeroizing::new(secret))),
        Err(Error::NoEntry) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn store(value: String) -> Result<()> {
    let secret = Zeroizing::new(value.trim().to_owned());
    validate(&secret)?;
    entry()?.set_secret(secret.as_bytes())?;
    Ok(())
}

fn validate(value: &str) -> Result<()> {
    if value.len() < 16 || value.len() > 4096 {
        bail!("provider API key must contain between 16 and 4096 characters");
    }
    if value.chars().any(char::is_control) {
        bail!("provider API key must not contain control characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn validates_provider_key_shape_without_storing_it() {
        assert!(validate("short").is_err());
        assert!(validate("sk-ant-test-1234567890").is_ok());
        assert!(validate("sk-ant-test\n1234567890").is_err());
    }
}
