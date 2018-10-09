// Copyright (C) 2018 Vincent Ambo <mail@tazj.in>
//
// journaldriver is free software: you can redistribute it and/or
// modify it under the terms of the GNU General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! This file implements journaldriver, a small application that
//! forwards logs from journald (systemd's log facility) to
//! Stackdriver Logging.
//!
//! Log entries are read continously from journald and are forwarded
//! to Stackdriver in batches.
//!
//! Stackdriver Logging has a concept of monitored resources. In the
//! simplest case this monitored resource will be the GCE instance on
//! which journaldriver is running.
//!
//! Information about the instance, the project and required security
//! credentials are retrieved from Google's metadata instance on GCP.
//!
//! To run journaldriver on non-GCP machines, users must specify the
//! `GOOGLE_APPLICATION_CREDENTIALS`, `GOOGLE_CLOUD_PROJECT` and
//! `LOG_NAME` environment variables.

#[macro_use] extern crate failure;
#[macro_use] extern crate log;
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate serde_json;
#[macro_use] extern crate lazy_static;

extern crate chrono;
extern crate env_logger;
extern crate medallion;
extern crate serde;
extern crate systemd;
extern crate ureq;

use chrono::offset::LocalResult;
use chrono::prelude::*;
use failure::ResultExt;
use serde_json::{from_str, Value};
use std::env;
use std::fs::{self, File, rename};
use std::io::{self, Read, ErrorKind, Write};
use std::mem;
use std::path::PathBuf;
use std::process;
use std::time::{Duration, Instant};
use systemd::journal::*;

#[cfg(test)]
mod tests;

const LOGGING_SERVICE: &str = "https://logging.googleapis.com/google.logging.v2.LoggingServiceV2";
const ENTRIES_WRITE_URL: &str = "https://logging.googleapis.com/v2/entries:write";
const METADATA_TOKEN_URL: &str = "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
const METADATA_ID_URL: &str = "http://metadata.google.internal/computeMetadata/v1/instance/id";
const METADATA_ZONE_URL: &str = "http://metadata.google.internal/computeMetadata/v1/instance/zone";
const METADATA_PROJECT_URL: &str = "http://metadata.google.internal/computeMetadata/v1/project/project-id";

/// Convenience type alias for results using failure's `Error` type.
type Result<T> = std::result::Result<T, failure::Error>;

/// Representation of static service account credentials for GCP.
#[derive(Debug, Deserialize)]
struct Credentials {
    /// PEM encoded private key
    private_key: String,

    /// `kid` of this private key
    private_key_id: String,

    /// "email" address of the service account
    client_email: String,
}

lazy_static! {
    /// ID of the GCP project to which to send logs.
    static ref PROJECT_ID: String = get_project_id();

    /// Name of the log to write to (this should only be manually
    /// configured if not running on GCP):
    static ref LOG_NAME: String = env::var("LOG_NAME")
        .unwrap_or("journaldriver".into());

    /// Service account credentials (if configured)
    static ref SERVICE_ACCOUNT_CREDENTIALS: Option<Credentials> =
        env::var("GOOGLE_APPLICATION_CREDENTIALS").ok()
        .and_then(|path| File::open(path).ok())
        .and_then(|file| serde_json::from_reader(file).ok());

    /// Descriptor of the currently monitored instance. Refer to the
    /// documentation of `determine_monitored_resource` for more
    /// information.
    static ref MONITORED_RESOURCE: Value = determine_monitored_resource();

    /// Path to the file in which journaldriver should persist its
    /// cursor state.
    static ref CURSOR_FILE: PathBuf = env::var("CURSOR_POSITION_FILE")
        .unwrap_or("/var/lib/journaldriver/cursor.pos".into())
        .into();

    /// Path to the temporary file used for cursor position writes.
    static ref CURSOR_TMP_FILE: PathBuf = {
        let mut tmp_path = CURSOR_FILE.clone();
        tmp_path.set_extension("pos.tmp");
        tmp_path
    };
}

/// Convenience helper for retrieving values from the metadata server.
fn get_metadata(url: &str) -> Result<String> {
    let response = ureq::get(url)
        .set("Metadata-Flavor", "Google")
        .timeout_connect(5000)
        .timeout_read(5000)
        .call();

    if response.ok() {
        // Whitespace is trimmed to remove newlines from responses.
        let body = response.into_string()
            .context("Failed to decode metadata response")?
            .trim().to_string();

        Ok(body)
    } else {
        let status = response.status_line().to_string();
        let body = response.into_string()
            .unwrap_or_else(|e| format!("Metadata body error: {}", e));
        bail!("Metadata failure: {} ({})", body, status)
    }
}

/// Convenience helper for determining the project ID.
fn get_project_id() -> String {
    env::var("GOOGLE_CLOUD_PROJECT")
        .map_err(Into::into)
        .or_else(|_: failure::Error| get_metadata(METADATA_PROJECT_URL))
        .expect("Could not determine project ID")
}

/// Determines the monitored resource descriptor used in Stackdriver
/// logs. On GCP this will be set to the instance ID as returned by
/// the metadata server.
///
/// On non-GCP machines the value is determined by using the
/// `GOOGLE_CLOUD_PROJECT` and `LOG_NAME` environment variables.
fn determine_monitored_resource() -> Value {
    if let Ok(log) = env::var("LOG_STREAM") {
        json!({
            "type": "logging_log",
            "labels": {
                "project_id": PROJECT_ID.as_str(),
                "name": log,
            }
        })
    } else {
        let instance_id = get_metadata(METADATA_ID_URL)
            .expect("Could not determine instance ID");

        let zone = get_metadata(METADATA_ZONE_URL)
            .expect("Could not determine instance zone");

        json!({
            "type": "gce_instance",
            "labels": {
                "project_id": PROJECT_ID.as_str(),
                "instance_id": instance_id,
                "zone": zone,
            }
        })
    }
}

/// Represents the response returned by the metadata server's token
/// endpoint. The token is normally valid for an hour.
#[derive(Deserialize)]
struct TokenResponse {
    expires_in: u64,
    access_token: String,
}

/// Struct used to store a token together with a sensible
/// representation of when it expires.
struct Token {
    token: String,
    fetched_at: Instant,
    expires: Duration,
}

impl Token {
    /// Does this token need to be renewed?
    fn is_expired(&self) -> bool {
        self.fetched_at.elapsed() > self.expires
    }
}

/// Retrieves a token from the GCP metadata service. Retrieving these
/// tokens requires no additional authentication.
fn get_metadata_token() -> Result<Token> {
    let body = get_metadata(METADATA_TOKEN_URL)?;
    let token: TokenResponse = from_str(&body)?;

    debug!("Fetched new token from metadata service");

    Ok(Token {
        fetched_at: Instant::now(),
        expires: Duration::from_secs(token.expires_in / 2),
        token: token.access_token,
    })
}

/// Signs a token using static client credentials configured for a
/// service account. This service account must have been given the
/// `Log Writer` role in Google Cloud IAM.
///
/// The process for creating and signing these tokens is described
/// here:
///
/// https://developers.google.com/identity/protocols/OAuth2ServiceAccount#jwt-auth
fn sign_service_account_token(credentials: &Credentials) -> Result<Token> {
    use medallion::{Algorithm, Header, Payload};

    let iat = Utc::now();
    let exp = iat.checked_add_signed(chrono::Duration::seconds(3600))
        .ok_or_else(|| format_err!("Failed to calculate token expiry"))?;

    let header = Header {
        alg: Algorithm::RS256,
        headers: Some(json!({
            "kid": credentials.private_key_id,
        })),
    };

    let payload: Payload<()> = Payload {
        iss: Some(credentials.client_email.clone()),
        sub: Some(credentials.client_email.clone()),
        aud: Some(LOGGING_SERVICE.to_string()),
        iat: Some(iat.timestamp() as u64),
        exp: Some(exp.timestamp() as u64),
        ..Default::default()
    };

    let token = medallion::Token::new(header, payload)
        .sign(credentials.private_key.as_bytes())
        .context("Signing service account token failed")?;

    debug!("Signed new service account token");

    Ok(Token {
        token,
        fetched_at: Instant::now(),
        expires: Duration::from_secs(3000),
    })
}

/// Retrieve the authentication token either by using static client
/// credentials, or by talking to the metadata server.
///
/// Which behaviour is used is controlled by the environment variable
/// `GOOGLE_APPLICATION_CREDENTIALS`, which should be configured to
/// point at a JSON private key file if service account authentication
/// is to be used.
fn get_token() -> Result<Token> {
    if let Some(credentials) = SERVICE_ACCOUNT_CREDENTIALS.as_ref() {
        sign_service_account_token(credentials)
    } else {
        get_metadata_token()
    }
}

/// This structure represents the different types of payloads
/// supported by journaldriver.
///
/// Currently log entries can either contain plain text messages or
/// structured payloads in JSON-format.
#[derive(Debug, Serialize, PartialEq)]
#[serde(untagged)]
enum Payload {
    TextPayload {
        #[serde(rename = "textPayload")]
        text_payload: String,
    },
    JsonPayload {
        #[serde(rename = "jsonPayload")]
        json_payload: Value,
    },
}

/// Attempt to parse a log message as JSON and return it as a
/// structured payload. If parsing fails, return the entry in plain
/// text format.
fn message_to_payload(message: Option<String>) -> Payload {
    match message {
        None => Payload::TextPayload { text_payload: "empty log entry".into() },
        Some(text_payload) => {
            // Attempt to deserialize the text payload as a generic
            // JSON value.
            if let Ok(json_payload) = serde_json::from_str::<Value>(&text_payload) {
                // If JSON-parsing succeeded on the payload, check
                // whether we parsed an object (Stackdriver does not
                // expect other types of JSON payload) and return it
                // in that case.
                if json_payload.is_object() {
                    return Payload::JsonPayload { json_payload }
                }
            }

            Payload::TextPayload { text_payload }
        }
    }
}

/// Attempt to parse journald's microsecond timestamps into a UTC
/// timestamp.
///
/// Parse errors are dismissed and returned as empty options: There
/// simply aren't any useful fallback mechanisms other than defaulting
/// to ingestion time for journaldriver's use-case.
fn parse_microseconds(input: String) -> Option<DateTime<Utc>> {
    if input.len() != 16 {
        return None;
    }

    let seconds: i64 = (&input[..10]).parse().ok()?;
    let micros: u32 = (&input[10..]).parse().ok()?;

    match Utc.timestamp_opt(seconds, micros * 1000) {
        LocalResult::Single(time) => Some(time),
        _ => None,
    }
}

/// Converts a journald log message priority to a
/// Stackdriver-compatible severity number.
///
/// Both Stackdriver and journald specify equivalent
/// severities/priorities. Conveniently, the names are the same.
/// Inconveniently, the numbers are not.
///
/// For more information on the journald priorities, consult these
/// man-pages:
///
/// * systemd.journal-fields(7) (section 'PRIORITY')
/// * sd-daemon(3)
/// * systemd.exec(5) (section 'SyslogLevelPrefix')
///
/// Note that priorities can be logged by applications via the prefix
/// concept described in these man pages, without interfering with
/// structured JSON-payloads.
///
/// For more information on the Stackdriver severity levels, please
/// consult Google's documentation:
///
/// https://cloud.google.com/logging/docs/reference/v2/rest/v2/LogEntry#LogSeverity
///
/// Any unknown priority values result in no severity being set.
fn priority_to_severity(priority: String) -> Option<u32> {
    match priority.as_ref() {
        "0" => Some(800), // emerg
        "1" => Some(700), // alert
        "2" => Some(600), // crit
        "3" => Some(500), // err
        "4" => Some(400), // warning
        "5" => Some(300), // notice
        "6" => Some(200), // info
        "7" => Some(100), // debug
        _ => None,
    }
}

/// This structure represents a log entry in the format expected by
/// the Stackdriver API.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEntry {
    labels: Value,

    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<DateTime<Utc>>,

    #[serde(flatten)]
    payload: Payload,

    // https://cloud.google.com/logging/docs/reference/v2/rest/v2/LogEntry#LogSeverity
    #[serde(skip_serializing_if = "Option::is_none")]
    severity: Option<u32>,
}

impl From<JournalRecord> for LogEntry {
    // Converts from the fields contained in a journald record to the
    // representation required by Stackdriver Logging.
    //
    // The fields are documented in systemd.journal-fields(7).
    fn from(mut record: JournalRecord) -> LogEntry {
        // The message field is technically just a convention, but
        // journald seems to default to it when ingesting unit
        // output.
        let payload = message_to_payload(record.remove("MESSAGE"));

        // Presumably this is always set, but who can be sure
        // about anything in this world.
        let hostname = record.remove("_HOSTNAME");

        // The unit is seemingly missing on kernel entries, but
        // present on all others.
        let unit = record.remove("_SYSTEMD_UNIT");

        // The source timestamp (if present) is specified in
        // microseconds since epoch.
        //
        // If it is not present or can not be parsed, journaldriver
        // will not send a timestamp for the log entry and it will
        // default to the ingestion time.
        let timestamp = record
            .remove("_SOURCE_REALTIME_TIMESTAMP")
            .and_then(parse_microseconds);

        // Journald uses syslogd's concept of priority. No idea if this is
        // always present, but it's optional in the Stackdriver API, so we just
        // omit it if we can't find or parse it.
        let severity = record
            .remove("PRIORITY")
            .and_then(priority_to_severity);

        LogEntry {
            payload,
            timestamp,
            labels: json!({
                "host": hostname,
                "unit": unit.unwrap_or_else(|| "syslog".into()),
            }),
            severity,
        }
    }
}

/// Attempt to read from the journal. If no new entry is present,
/// await the next one up to the specified timeout.
fn receive_next_record(timeout: Duration, journal: &mut Journal)
                       -> Result<Option<JournalRecord>> {
    let next_record = journal.next_record()?;
    if next_record.is_some() {
        return Ok(next_record);
    }

    Ok(journal.await_next_record(Some(timeout))?)
}

/// This function starts a double-looped, blocking receiver. It will
/// buffer messages for half a second before flushing them to
/// Stackdriver.
fn receiver_loop(mut journal: Journal) -> Result<()> {
    let mut token = get_token()?;

    let mut buf: Vec<LogEntry> = Vec::new();
    let iteration = Duration::from_millis(500);

    loop {
        trace!("Beginning outer iteration");
        let now = Instant::now();

        loop {
            if now.elapsed() > iteration {
                break;
            }

            if let Ok(Some(entry)) = receive_next_record(iteration, &mut journal) {
                trace!("Received a new entry");
                buf.push(entry.into());
            }
        }

        if !buf.is_empty() {
            let to_flush = mem::replace(&mut buf, Vec::new());
            flush(&mut token, to_flush, journal.cursor()?)?;
        }

        trace!("Done outer iteration");
    }
}

/// Writes the current cursor into `/var/journaldriver/cursor.pos`. To
/// avoid issues with journaldriver being terminated while the cursor
/// is still being written, this will first write the cursor into a
/// temporary file and then move it.
fn persist_cursor(cursor: String) -> Result<()> {
    // This code exists to aid in tracking down if there are other
    // causes of issue #2 than what has already been taken care of.
    //
    // One theory is that journald (or the Rust library to interface
    // with it) may occasionally return empty cursor strings. If this
    // is ever the case, we would like to know about it.
    if cursor.is_empty() {
        error!("Received empty journald cursor position, refusing to persist!");
        error!("Please report this message at https://github.com/tazjin/journaldriver/issues/2");
        return Ok(())
    }

    let mut file = File::create(&*CURSOR_TMP_FILE)?;
    write!(file, "{}", cursor).context("Failed to write cursor file")?;
    rename(&*CURSOR_TMP_FILE, &*CURSOR_FILE)
        .context("Failed to move cursor file")
        .map_err(Into::into)
}

/// Flushes all drained records to Stackdriver. Any Stackdriver
/// message can at most contain 1000 log entries which means they are
/// chunked up here.
///
/// In some cases large payloads seem to cause errors in Stackdriver -
/// the chunks are therefore made smaller here.
///
/// If flushing is successful the last cursor position will be
/// persisted to disk.
fn flush(token: &mut Token,
         entries: Vec<LogEntry>,
         cursor: String) -> Result<()> {
    if token.is_expired() {
        debug!("Refreshing Google metadata access token");
        let new_token = get_token()?;
        mem::replace(token, new_token);
    }

    for chunk in entries.chunks(750) {
        let request = prepare_request(chunk);
        if let Err(write_error) = write_entries(token, request) {
            error!("Failed to write {} entries: {}", chunk.len(), write_error)
        } else {
            debug!("Wrote {} entries to Stackdriver", chunk.len())
        }
    }

    persist_cursor(cursor)
}

/// Convert a slice of log entries into the format expected by
/// Stackdriver. This format is documented here:
///
/// https://cloud.google.com/logging/docs/reference/v2/rest/v2/entries/write
fn prepare_request(entries: &[LogEntry]) -> Value {
    json!({
        "logName": format!("projects/{}/logs/{}", PROJECT_ID.as_str(), LOG_NAME.as_str()),
        "resource": &*MONITORED_RESOURCE,
        "entries": entries,
        "partialSuccess": true
    })
}

/// Perform the log entry insertion in Stackdriver Logging.
fn write_entries(token: &Token, request: Value) -> Result<()> {
    let response = ureq::post(ENTRIES_WRITE_URL)
        .set("Authorization", format!("Bearer {}", token.token).as_str())
        // The timeout values are set relatively high, not because of
        // an expectation of Stackdriver being slow but just to
        // eventually hit an error case in case of network troubles.
        // Presumably no request in a functioning environment will
        // ever hit these limits.
        .timeout_connect(2000)
        .timeout_read(5000)
        .send_json(request);

    if response.ok() {
        Ok(())
    } else {
        let status = response.status_line().to_string();
        let body = response.into_string()
            .unwrap_or_else(|_| "no response body".into());
        bail!("Write failure: {} ({})", body, status)
    }
}

/// Attempt to read the initial cursor position from the configured
/// file. If there is no initial cursor position set, read from the
/// tail of the log.
///
/// The only "acceptable" error when reading the cursor position is
/// the cursor position file not existing, other errors are fatal
/// because they indicate a misconfiguration of journaldriver.
fn initial_cursor() -> Result<JournalSeek> {
    let read_result: io::Result<String> = (|| {
        let mut contents = String::new();
        let mut file = File::open(&*CURSOR_FILE)?;
        file.read_to_string(&mut contents)?;
        Ok(contents.trim().into())
    })();

    match read_result {
        Ok(cursor) => Ok(JournalSeek::Cursor { cursor }),
        Err(ref err) if err.kind() == ErrorKind::NotFound => {
            info!("No previous cursor position, reading from journal tail");
            Ok(JournalSeek::Tail)
        },
        Err(err) => {
            (Err(err).context("Could not read cursor position"))?
        }
    }
}

fn main () {
    env_logger::init();

    // If the cursor file does not yet exist, the directory structure
    // leading up to it should be created:
    let cursor_position_dir = CURSOR_FILE.parent()
        .expect("Invalid cursor position file path");

    fs::create_dir_all(cursor_position_dir)
        .expect("Could not create directory to store cursor position in");

    let mut journal = Journal::open(JournalFiles::All, false, true)
        .expect("Failed to open systemd journal");

    let seek_position = initial_cursor()
        .expect("Failed to determine initial cursor position");

    match journal.seek(seek_position) {
        Ok(cursor) => info!("Opened journal at cursor '{}'", cursor),
        Err(err) => {
            error!("Failed to set initial journal position: {}", err);
            process::exit(1)
        }
    }

    receiver_loop(journal).expect("log receiver encountered an unexpected error");
}
