//! `group.json` against the serializer the C# apps (0.2.4) read and write it with: the .NET Framework
//! DataContractJsonSerializer, driven through Windows PowerShell 5.1 (which runs on that same framework).
//! The file this crate writes must load there, and the file written there must load here.
//!
//! Windows only, and skipped (with a note) where PowerShell cannot compile the little C# mirror of the data
//! contract, e.g. on a locked-down machine. Nothing but two files in a temp folder is touched.

#![cfg(windows)]

use std::collections::HashMap;
use std::process::Command;

use hdrcast_core::{GroupConfig, Invite};

/// The data contract of shared/Group/GroupConfig.cs, member for member (C# 5, so PowerShell's compiler takes
/// it), and the two halves of shared/Common/Json.cs that touch the file. Strings are printed as UTF-16 code
/// units in hex: the console's code page must not come between the two sides.
const SCRIPT: &str = r#"
param([string]$In, [string]$Out)
$ErrorActionPreference = 'Stop'
Add-Type -ReferencedAssemblies System.Runtime.Serialization, System.Xml -TypeDefinition @'
using System.IO;
using System.Runtime.Serialization;
using System.Runtime.Serialization.Json;
using System.Text;
using System.Xml;

[DataContract]
public sealed class GroupConfig
{
    [DataMember(Name = "v", Order = 0)] public int Version { get; set; }
    [DataMember(Name = "signal", Order = 1)] public string SignalUrl { get; set; }
    [DataMember(Name = "gid", Order = 2)] public string GroupId { get; set; }
    [DataMember(Name = "key", Order = 3)] public string GroupKey { get; set; }
    [DataMember(Name = "name", Order = 4)] public string GroupName { get; set; }
    [DataMember(Name = "me", Order = 5)] public string DisplayName { get; set; }
    [DataMember(Name = "peer", Order = 6)] public string PeerId { get; set; }
    [DataMember(Name = "importedAt", Order = 7)] public long ImportedAtMs { get; set; }
    [DataMember(Name = "detectClipboard", Order = 8)] public bool DetectClipboardInvites { get; set; }
    [DataMember(Name = "dismissed", Order = 9)] public string DismissedFingerprint { get; set; }

    [OnDeserializing]
    void OnDeserializing(StreamingContext _)
    {
        SignalUrl = GroupId = GroupKey = GroupName = DisplayName = PeerId = DismissedFingerprint = "";
        DetectClipboardInvites = true;
    }
}

public static class Dcjs
{
    static DataContractJsonSerializer Create()
    {
        return new DataContractJsonSerializer(typeof(GroupConfig), new DataContractJsonSerializerSettings { UseSimpleDictionaryFormat = true });
    }

    public static GroupConfig Load(string path)
    {
        string json = File.ReadAllText(path, Encoding.UTF8);
        using (var ms = new MemoryStream(Encoding.UTF8.GetBytes(json)))
            return (GroupConfig)Create().ReadObject(ms);
    }

    public static void Save(string path, GroupConfig value)
    {
        using (var fs = new FileStream(path, FileMode.Create, FileAccess.Write, FileShare.None))
        using (XmlDictionaryWriter w = JsonReaderWriterFactory.CreateJsonWriter(fs, new UTF8Encoding(false), false, true, "  "))
        {
            Create().WriteObject(w, value);
            w.Flush();
        }
    }
}
'@
function Hex([string]$s) { ($s.ToCharArray() | ForEach-Object { '{0:x4}' -f [int]$_ }) -join '' }
'READY'
$c = [Dcjs]::Load($In)
"v=$($c.Version)"
"signal=$(Hex $c.SignalUrl)"
"gid=$(Hex $c.GroupId)"
"key=$(Hex $c.GroupKey)"
"name=$(Hex $c.GroupName)"
"me=$(Hex $c.DisplayName)"
"peer=$(Hex $c.PeerId)"
"importedAt=$($c.ImportedAtMs)"
"detectClipboard=$($c.DetectClipboardInvites)"
"dismissed=$(Hex $c.DismissedFingerprint)"
[Dcjs]::Save($Out, $c)
'DONE'
"#;

fn hex_utf16(s: &str) -> String {
    s.encode_utf16().map(|unit| format!("{unit:04x}")).collect()
}

#[test]
fn group_json_round_trips_through_the_serializer_of_the_csharp_apps() {
    let dir = std::env::temp_dir().join(format!("hdrcast-interop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (ours, theirs, script) = (dir.join("rust.json"), dir.join("csharp.json"), dir.join("dcjs.ps1"));

    // names that need UTF-8, escaping and a surrogate pair; the key is the public one of the test vectors
    let key: Vec<u8> = (0u8..32).collect();
    let invite = Invite::new("http://101.43.23.94:18650", "oKGio6SlpqeoqaqrrK2urw", &key, "测试组 \"quoted\" \\ /slash").unwrap();
    let mut cfg = GroupConfig::default().normalize().adopt(&invite, "小红 😀");
    cfg.dismissed_fingerprint = invite.fingerprint();
    cfg.detect_clipboard_invites = false; // not the default, so a dropped field would show
    cfg.save(&ours).unwrap();
    std::fs::write(&script, SCRIPT).unwrap();

    let run = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-In")
        .arg(&ours)
        .arg("-Out")
        .arg(&theirs)
        .output();
    let output = match run {
        Ok(output) => output,
        Err(e) => {
            eprintln!("skipped: Windows PowerShell could not be started ({e})");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.lines().any(|l| l.trim() == "READY") {
        eprintln!("skipped: the C# data contract could not be compiled here\n{stderr}");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    assert!(output.status.success() && stdout.lines().any(|l| l.trim() == "DONE"), "the C# side failed:\n{stdout}\n{stderr}");

    // what the C# apps see in the file this crate wrote
    let seen: HashMap<&str, &str> = stdout.lines().filter_map(|l| l.trim().split_once('=')).collect();
    assert_eq!(seen["v"], "1");
    assert_eq!(seen["signal"], hex_utf16(&cfg.signal_url));
    assert_eq!(seen["gid"], hex_utf16(&cfg.group_id));
    assert_eq!(seen["key"], hex_utf16(&cfg.group_key));
    assert_eq!(seen["name"], hex_utf16(&cfg.group_name));
    assert_eq!(seen["me"], hex_utf16(&cfg.display_name));
    assert_eq!(seen["peer"], hex_utf16(&cfg.peer_id));
    assert_eq!(seen["importedAt"], cfg.imported_at_ms.to_string());
    assert_eq!(seen["detectClipboard"], "False");
    assert_eq!(seen["dismissed"], hex_utf16(&cfg.dismissed_fingerprint));

    // and what this crate sees in the file they wrote
    assert_eq!(GroupConfig::load(&theirs), cfg);
    let raw = std::fs::read_to_string(&theirs).unwrap();
    assert!(raw.contains("\r\n  \"signal\": \"http:\\/\\/101.43.23.94:18650\""), "an unexpected layout:\n{raw}");
    std::fs::remove_dir_all(&dir).unwrap();
}
