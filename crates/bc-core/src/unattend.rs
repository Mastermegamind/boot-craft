//! The Windows setup options: an `autounattend.xml` at the root of the
//! stick, which Windows Setup reads by itself. Each option adds only its own
//! settings; with none chosen, no file is written and Setup runs as usual.

use std::fmt::Write as _;

/// What Windows Setup should do differently. All off by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Install where Windows 11 says the PC is unsupported: no TPM 2.0, no
    /// Secure Boot, under 4 GB of RAM (Setup's LabConfig keys).
    pub bypass_requirements: bool,
    /// Make this local account (an administrator, with no password until the
    /// first sign-in asks for one) instead of signing in to a Microsoft
    /// account, and allow setup without internet.
    pub local_account: Option<String>,
    /// Answer the privacy questions with "no" to everything.
    pub skip_privacy: bool,
    /// Language and region, as a locale name: "en-US", "fr-FR".
    pub locale: Option<String>,
    /// Stop Windows from turning on BitLocker device encryption by itself.
    pub no_bitlocker: bool,
}

impl Options {
    pub fn any(&self) -> bool {
        *self != Options::default()
    }

    /// What's chosen, in words.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.bypass_requirements {
            out.push("skip the TPM, Secure Boot and RAM checks".to_string());
        }
        if let Some(name) = &self.local_account {
            out.push(format!("create the local account \"{name}\" (no Microsoft account, no internet needed)"));
        }
        if self.skip_privacy {
            out.push("answer the privacy questions with no".to_string());
        }
        if let Some(locale) = &self.locale {
            out.push(format!("language and region {locale}"));
        }
        if self.no_bitlocker {
            out.push("don't turn on BitLocker device encryption automatically".to_string());
        }
        out
    }

    /// Refuses what Windows wouldn't accept.
    pub fn check(&self) -> Result<(), String> {
        if let Some(name) = &self.local_account {
            check_account(name)?;
        }
        if let Some(locale) = &self.locale {
            check_locale(locale)?;
        }
        Ok(())
    }
}

/// Windows local account names: 1 to 20 characters, none of `"/\[]:;|=,+*?<>@`,
/// not only dots and spaces, and not a built-in account.
fn check_account(name: &str) -> Result<(), String> {
    let bad = |c: char| "\"/\\[]:;|=,+*?<>@%".contains(c) || c.is_control();
    if name.is_empty() || name.chars().count() > 20 {
        return Err("a Windows account name is 1 to 20 characters".into());
    }
    if let Some(c) = name.chars().find(|&c| bad(c)) {
        return Err(format!("a Windows account name can't contain {c:?}"));
    }
    if name.chars().all(|c| c == '.' || c == ' ') {
        return Err("a Windows account name can't be only dots and spaces".into());
    }
    let reserved = ["administrator", "guest", "defaultaccount", "wdagutilityaccount", "system", "none"];
    if reserved.contains(&name.to_lowercase().as_str()) {
        return Err(format!("\"{name}\" is one of Windows' own accounts; choose another name"));
    }
    Ok(())
}

/// "en-US", "sr-Latn-RS": a language, then optional script and region.
fn check_locale(locale: &str) -> Result<(), String> {
    let mut parts = locale.split('-');
    let language = parts.next().unwrap_or("");
    let fine = (2..=3).contains(&language.len())
        && language.chars().all(|c| c.is_ascii_lowercase())
        && parts.all(|p| (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()));
    if fine { Ok(()) } else { Err(format!("\"{locale}\" isn't a locale name like en-US or fr-FR")) }
}

/// `processorArchitecture` values: amd64, arm64 or x86.
fn arch(arch: Option<&str>) -> &'static str {
    match arch {
        Some("arm64") => "arm64",
        Some("x86") => "x86",
        _ => "amd64",
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The `autounattend.xml` for `options` on a Windows of architecture
/// `arch` ("amd64", "arm64", "x86"), or `None` when nothing is chosen.
pub fn xml(options: &Options, arch_name: Option<&str>) -> Option<String> {
    if !options.any() {
        return None;
    }
    let arch = arch(arch_name);
    let component = |name: &str| {
        format!(
            "    <component name=\"{name}\" processorArchitecture=\"{arch}\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" \
             versionScope=\"nonSxS\" xmlns:wcm=\"http://schemas.microsoft.com/WMIConfig/2002/State\" \
             xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n"
        )
    };
    let commands = |tag: &str, list: &[String]| {
        let mut out = format!("      <{tag}>\n");
        for (i, command) in list.iter().enumerate() {
            let (item, field) =
                if tag == "FirstLogonCommands" { ("SynchronousCommand", "CommandLine") } else { ("RunSynchronousCommand", "Path") };
            let _ = write!(
                out,
                "        <{item} wcm:action=\"add\">\n          <Order>{}</Order>\n          <{field}>{}</{field}>\n        </{item}>\n",
                i + 1,
                escape(command)
            );
        }
        let _ = writeln!(out, "      </{tag}>");
        out
    };
    let reg = |key: &str, value: &str| format!("reg add {key} /v {value} /t REG_DWORD /d 1 /f");

    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<unattend xmlns=\"urn:schemas-microsoft-com:unattend\">\n");

    // windowsPE: Setup itself, before anything is installed.
    let mut pe = Vec::new();
    if options.bypass_requirements {
        for value in ["BypassTPMCheck", "BypassSecureBootCheck", "BypassRAMCheck", "BypassStorageCheck", "BypassCPUCheck"] {
            pe.push(reg("HKLM\\SYSTEM\\Setup\\LabConfig", value));
        }
    }
    if pe.len() + usize::from(options.locale.is_some()) > 0 {
        xml.push_str("  <settings pass=\"windowsPE\">\n");
        if let Some(locale) = &options.locale {
            xml.push_str(&component("Microsoft-Windows-International-Core-WinPE"));
            let locale = escape(locale);
            let _ = write!(
                xml,
                "      <SetupUILanguage>\n        <UILanguage>{locale}</UILanguage>\n      </SetupUILanguage>\n      <InputLocale>{locale}</InputLocale>\n      \
                 <SystemLocale>{locale}</SystemLocale>\n      <UILanguage>{locale}</UILanguage>\n      <UserLocale>{locale}</UserLocale>\n    </component>\n"
            );
        }
        if !pe.is_empty() {
            xml.push_str(&component("Microsoft-Windows-Setup"));
            // An empty key: Setup still asks which edition to install.
            xml.push_str("      <UserData>\n        <ProductKey>\n          <Key></Key>\n        </ProductKey>\n      </UserData>\n");
            xml.push_str(&commands("RunSynchronous", &pe));
            xml.push_str("    </component>\n");
        }
        xml.push_str("  </settings>\n");
    }

    // specialize: the installed system, before its first start.
    let mut special = Vec::new();
    if options.local_account.is_some() {
        special.push(reg("HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\OOBE", "BypassNRO"));
    }
    if options.no_bitlocker {
        special.push(reg("HKLM\\SYSTEM\\CurrentControlSet\\Control\\BitLocker", "PreventDeviceEncryption"));
    }
    if !special.is_empty() {
        xml.push_str("  <settings pass=\"specialize\">\n");
        xml.push_str(&component("Microsoft-Windows-Deployment"));
        xml.push_str(&commands("RunSynchronous", &special));
        xml.push_str("    </component>\n  </settings>\n");
    }

    // oobeSystem: the first-start questions.
    if options.local_account.is_some() || options.skip_privacy || options.locale.is_some() {
        xml.push_str("  <settings pass=\"oobeSystem\">\n");
        if let Some(locale) = &options.locale {
            xml.push_str(&component("Microsoft-Windows-International-Core"));
            let locale = escape(locale);
            let _ = write!(
                xml,
                "      <InputLocale>{locale}</InputLocale>\n      <SystemLocale>{locale}</SystemLocale>\n      <UILanguage>{locale}</UILanguage>\n      \
                 <UserLocale>{locale}</UserLocale>\n    </component>\n"
            );
        }
        if options.local_account.is_some() || options.skip_privacy {
            xml.push_str(&component("Microsoft-Windows-Shell-Setup"));
            xml.push_str("      <OOBE>\n");
            if options.local_account.is_some() {
                xml.push_str("        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>\n");
            }
            if options.skip_privacy {
                xml.push_str("        <ProtectYourPC>3</ProtectYourPC>\n");
            }
            xml.push_str("      </OOBE>\n");
            if let Some(name) = &options.local_account {
                let name = escape(name);
                let _ = write!(
                    xml,
                    "      <UserAccounts>\n        <LocalAccounts>\n          <LocalAccount wcm:action=\"add\">\n            <Name>{name}</Name>\n            \
                     <DisplayName>{name}</DisplayName>\n            <Group>Administrators</Group>\n            <Password>\n              <Value></Value>\n              \
                     <PlainText>true</PlainText>\n            </Password>\n          </LocalAccount>\n        </LocalAccounts>\n      </UserAccounts>\n"
                );
                // The first sign-in asks for a password.
                let first = format!("net user \"{}\" /logonpasswordchg:yes", options.local_account.as_deref().unwrap_or_default());
                xml.push_str(&commands("FirstLogonCommands", &[first]));
            }
            xml.push_str("    </component>\n");
        }
        xml.push_str("  </settings>\n");
    }
    xml.push_str("</unattend>\n");
    Some(xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_chosen_writes_no_file() {
        assert_eq!(xml(&Options::default(), Some("amd64")), None);
    }

    #[test]
    fn bypass_only() {
        let options = Options { bypass_requirements: true, ..Options::default() };
        assert_eq!(
            xml(&options, Some("amd64")).unwrap(),
            r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
      <UserData>
        <ProductKey>
          <Key></Key>
        </ProductKey>
      </UserData>
      <RunSynchronous>
        <RunSynchronousCommand wcm:action="add">
          <Order>1</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassTPMCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>2</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassSecureBootCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>3</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassRAMCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>4</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassStorageCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>5</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassCPUCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
      </RunSynchronous>
    </component>
  </settings>
</unattend>
"#
        );
    }

    #[test]
    fn local_account_and_privacy() {
        let options = Options { local_account: Some("Ada & Co".into()), skip_privacy: true, ..Options::default() };
        let xml = xml(&options, Some("arm64")).unwrap();
        assert!(!xml.contains("windowsPE"), "nothing for Setup itself");
        assert!(xml.contains(r#"<settings pass="specialize">"#));
        assert!(xml.contains(r"reg add HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\OOBE /v BypassNRO /t REG_DWORD /d 1 /f"));
        assert!(xml.contains("<HideOnlineAccountScreens>true</HideOnlineAccountScreens>"));
        assert!(xml.contains("<ProtectYourPC>3</ProtectYourPC>"));
        assert!(xml.contains("<Name>Ada &amp; Co</Name>"));
        assert!(xml.contains("<Group>Administrators</Group>"));
        assert!(xml.contains("<CommandLine>net user &quot;Ada &amp; Co&quot; /logonpasswordchg:yes</CommandLine>"));
        assert!(xml.contains(r#"processorArchitecture="arm64""#));
        assert_eq!(xml.matches("<component ").count(), xml.matches("</component>").count());
        assert_eq!(xml.matches("<settings ").count(), 2);
    }

    #[test]
    fn locale_and_bitlocker() {
        let options = Options { locale: Some("fr-FR".into()), no_bitlocker: true, ..Options::default() };
        let xml = xml(&options, None).unwrap();
        assert!(xml.contains("Microsoft-Windows-International-Core-WinPE"));
        assert!(xml.contains("<UILanguage>fr-FR</UILanguage>"));
        assert!(xml.contains(r#"<component name="Microsoft-Windows-International-Core" "#));
        assert!(xml.contains(r"reg add HKLM\SYSTEM\CurrentControlSet\Control\BitLocker /v PreventDeviceEncryption /t REG_DWORD /d 1 /f"));
        assert!(!xml.contains("Microsoft-Windows-Setup\""), "no Setup component without bypass");
    }

    #[test]
    fn names_windows_wouldnt_take() {
        assert!(check_account("ada").is_ok());
        assert!(check_account("").is_err());
        assert!(check_account("a/b").is_err());
        assert!(check_account("Administrator").is_err());
        assert!(check_account("...").is_err());
        assert!(check_account("abcdefghijklmnopqrstu").is_err());
        assert!(check_locale("en-US").is_ok());
        assert!(check_locale("sr-Latn-RS").is_ok());
        assert!(check_locale("EN").is_err());
        assert!(check_locale("en_US").is_err());
        assert!(check_locale("en-").is_err());
    }
}
