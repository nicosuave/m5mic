use anyhow::{anyhow, Context, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition, EspNvs};

const NAMESPACE: &str = "m5mic";
const KEY_SSID: &str = "wifi_ssid";
const KEY_PASS: &str = "wifi_pass";

#[derive(Clone)]
pub struct WifiStore {
    partition: EspDefaultNvsPartition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WifiCredentials {
    pub ssid: String,
    pub password: String,
}

impl WifiStore {
    pub fn new(partition: EspDefaultNvsPartition) -> Self {
        Self { partition }
    }

    pub fn load(&self) -> Result<Option<WifiCredentials>> {
        let nvs = self.open().context("open wifi config nvs")?;
        let mut ssid_buf = [0u8; 33];
        let Some(ssid) = nvs
            .get_str(KEY_SSID, &mut ssid_buf)
            .context("read wifi ssid")?
        else {
            return Ok(None);
        };

        if ssid.is_empty() {
            return Ok(None);
        }

        let mut pass_buf = [0u8; 65];
        let password = nvs
            .get_str(KEY_PASS, &mut pass_buf)
            .context("read wifi password")?
            .unwrap_or("");

        Ok(Some(WifiCredentials {
            ssid: ssid.to_string(),
            password: password.to_string(),
        }))
    }

    pub fn save(&self, credentials: &WifiCredentials) -> Result<()> {
        validate_credentials(credentials)?;
        let nvs = self.open().context("open wifi config nvs")?;
        nvs.set_str(KEY_SSID, credentials.ssid.as_str())
            .context("save wifi ssid")?;
        nvs.set_str(KEY_PASS, credentials.password.as_str())
            .context("save wifi password")?;
        Ok(())
    }

    fn open(&self) -> Result<EspDefaultNvs> {
        EspNvs::new(self.partition.clone(), NAMESPACE, true).map_err(Into::into)
    }
}

pub fn fallback_credentials() -> Result<WifiCredentials> {
    let ssid = option_env!("WIFI_SSID").ok_or_else(|| anyhow!("WIFI_SSID is required"))?;
    let password = option_env!("WIFI_PASS").ok_or_else(|| anyhow!("WIFI_PASS is required"))?;
    let credentials = WifiCredentials {
        ssid: ssid.to_string(),
        password: password.to_string(),
    };
    validate_credentials(&credentials)?;
    Ok(credentials)
}

pub fn validate_credentials(credentials: &WifiCredentials) -> Result<()> {
    if credentials.ssid.is_empty() {
        return Err(anyhow!("wifi ssid is required"));
    }
    if credentials.ssid.len() > 32 {
        return Err(anyhow!("wifi ssid is too long"));
    }
    if credentials.password.len() > 64 {
        return Err(anyhow!("wifi password is too long"));
    }
    Ok(())
}
