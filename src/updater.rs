use crate::{rulesets::ENABLE_MIXED_RULESETS, rulesets::RULE_ACTIVE_STATES, RuleSets, Storage, UpdateChannel, UpdateChannels};
use flate2::read::GzDecoder;
use http_req::request;
use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::rsa::Padding;
use openssl::sign::Verifier;
use serde_json::Value;
use std::cmp;
use std::error::Error;
use std::fmt;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

type Timestamp = usize;

#[derive(Debug, Clone)]
struct UpdaterError {
    error_string: String,
}

impl UpdaterError {
    pub fn new(error_string: String) -> UpdaterError {
        UpdaterError {
            error_string
        }
    }
}

impl fmt::Display for UpdaterError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.error_string)
    }
}

impl Error for UpdaterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}


pub struct Updater<'a> {
    rulesets: &'a mut RuleSets,
    update_channels: &'a UpdateChannels,
    storage: &'a Storage,
    default_rulesets: Option<String>,
    periodicity: usize,
}

impl<'a> Updater<'a> {
    /// Returns an updater with the rulesets, update channels, storage, and interval to check for
    /// new rulesets
    ///
    /// # Arguments
    ///
    /// * `rulesets` - A ruleset struct to update
    /// * `update_channels` - The update channels where to look for new rulesets
    /// * `storage` - The storage engine for key-value pairs
    /// * `default_rulesets` - An optional string representing the default rulesets, which may or
    /// may not be replaced by updates
    /// * `periodicity` - The interval to check for new rulesets
    pub fn new(rulesets: &'a mut RuleSets, update_channels: &'a UpdateChannels, storage: &'a Storage, default_rulesets: Option<String>, periodicity: usize) -> Updater<'a> {
        Updater {
            rulesets,
            update_channels,
            storage,
            default_rulesets,
            periodicity,
        }
    }

    /// Get the current timestamp in seconds
    fn current_timestamp() -> Timestamp {
	let since_the_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
	let current_timestamp = since_the_epoch.as_secs();
        current_timestamp as Timestamp
    }

    /// Returns an `Option<i32>` optional timestamp if there are new rulesets.  If no new rulesets
    /// are available, or if there is a failure for any reason, return `None`
    ///
    /// # Arguments
    ///
    /// * `uc` - The update channel to check for new rulesets on
    fn check_for_new_rulesets(&self, uc: &UpdateChannel) -> Option<Timestamp> {
        let mut writer = Vec::new();

        let res = match request::get(uc.update_path_prefix.clone() + "/latest-rulesets-timestamp", &mut writer) {
            Ok(result) => result,
            Err(_) => return None
        };

        if res.status_code().is_success() {
            let ts_string = match String::from_utf8(writer) {
                Ok(timestamp) => timestamp,
                Err(_) => return None
            };
            
            let timestamp: Timestamp = match ts_string.trim().parse() {
                Ok(num) => num,
                Err(_) => return None
            };

            let stored_timestamp: Timestamp = self.storage.get_int(String::from("rulesets-timestamp: ") + &uc.name);

            if stored_timestamp < timestamp {
                Some(timestamp)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Given an update channel and timestamp, this returns a result-wrapped tuple, the first value the first value is
    /// a `Vec<u8>` of the signature file, the second is a `Vec<u8>` of the rulesets file.
    ///
    /// # Arguments
    ///
    /// * `rulesets_timestamp` - The timestamp for the rulesets
    /// * `update_channel` - The update channel to download rulesets for
    fn get_new_rulesets(&self, rulesets_timestamp: Timestamp, update_channel: &UpdateChannel) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
        self.storage.set_int(String::from("rulesets-timestamp: ") + &update_channel.name, rulesets_timestamp);

        // TODO: Use futures to asynchronously fetch signature and rulesets

        let mut signature_writer = Vec::new();
        let signature_res = request::get(update_channel.update_path_prefix.clone() + "/rulesets-signature." + &rulesets_timestamp.to_string() + ".sha256", &mut signature_writer)?;

        if !signature_res.status_code().is_success() {
            return Err(Box::new(UpdaterError::new(format!("{}: A non-2XX response was returned from the ruleset signature URL", &update_channel.name))));
        }


        let mut rulesets_writer = Vec::new();
        let rulesets_res = request::get(update_channel.update_path_prefix.clone() + "/default.rulesets." + &rulesets_timestamp.to_string() + ".gz", &mut rulesets_writer)?;

        if !rulesets_res.status_code().is_success() {
            return Err(Box::new(UpdaterError::new(format!("{}: A non-2XX response was returned from the ruleset URL", &update_channel.name))));
        }

        Ok((signature_writer, rulesets_writer))
    }

    /// If the given signature for the given rulesets verifies with the key stored in the given
    /// update channel, store this update channel in the struct storage layer.  Returns a
    /// result-wrapped unit
    ///
    /// # Arguments
    ///
    /// * `signature` - A SHA256 RSA PSS signature
    /// * `rulesets` - Rulesets to check the signature for
    /// * `rulesets_timestamp` - The timestamp for the rulesets, which we use to verify that it
    /// matches the timestamp in the signed rulesets JSON
    /// * `update_channel` - Contains the key which we verify the signatures with
    fn verify_and_store_new_rulesets(&mut self, signature: Vec<u8>, rulesets: Vec<u8>, rulesets_timestamp: Timestamp, update_channel: &UpdateChannel) -> Result<(), Box<dyn Error>> {
        let update_channel_key = PKey::from_rsa(update_channel.key.clone())?;
        let mut verifier = Verifier::new(MessageDigest::sha256(), &update_channel_key)?;
        verifier.set_rsa_padding(Padding::PKCS1_PSS)?;

        verifier.update(&rulesets)?;

        if verifier.verify(&signature)? {
            info!("{}: Downloaded ruleset signature checks out.  Storing rulesets.", update_channel.name);

            let mut rulesets_json_string = String::new();
            let mut decoder = GzDecoder::new(&rulesets[..]);
            decoder.read_to_string(&mut rulesets_json_string)?;

            let rulesets_json_value: Value = serde_json::from_str(&rulesets_json_string)?;
            match rulesets_json_value.get("timestamp") {
                Some(Value::Number(json_timestamp)) if json_timestamp.is_i64() => {
                    if json_timestamp.as_i64().unwrap() != rulesets_timestamp as i64 {
                        return Err(Box::new(UpdaterError::new(format!("{}: JSON timestamp does not match with latest timestamp file", &update_channel.name))));
                    }
                },
                _ => {
                    return Err(Box::new(UpdaterError::new(format!("{}: Could not parse JSON timestamp", &update_channel.name))));
                }
            }

            self.storage.set_string(format!("rulesets: {}", update_channel.name), rulesets_json_string);
        } else {
            return Err(Box::new(UpdaterError::new(format!("{}: Downloaded ruleset signature is invalid.  Aborting.", &update_channel.name))));
        }

        Ok(())
    }

    /// Perform a check for updates.  For all ruleset update channels:
    ///
    /// 1. Check if new rulesets exist by requesting a defined endpoint for a timestamp, which is
    ///    compared to a stored timestamp
    /// 2. If new rulesets exist, download them along with a signature
    /// 3. Verify if the signature is valid, and if so...
    /// 4. Store the rulesets
    pub fn perform_check(&mut self) {
        info!("Checking for new rulesets.");

	self.storage.set_int(String::from("last-checked"), Self::current_timestamp());

	let extension_timestamp = self.storage.get_int(String::from("extension-timestamp"));

        let mut some_updated = false;
        for uc in self.update_channels.get_all() {
            if let Some(new_rulesets_timestamp) = self.check_for_new_rulesets(uc) {
                if uc.replaces_default_rulesets && extension_timestamp > new_rulesets_timestamp {
                    info!("{}: A new ruleset bundle has been released, but it is older than the extension-bundled rulesets it replaces.  Skipping.", uc.name);
                    continue;
                }
                info!("{}: A new ruleset bundle has been released.  Downloading now.", uc.name);

                let (signature, rulesets) = match self.get_new_rulesets(new_rulesets_timestamp, uc) {
                    Ok(rs_tuple) => rs_tuple,
                    Err(err) => {
                        error!("{:?}", err);
                        continue;
                    }
                };

                if let Err(err) = self.verify_and_store_new_rulesets(signature, rulesets, new_rulesets_timestamp, uc) {
                    error!("{:?}", err);
                    continue;
                }

                self.storage.set_int(format!("rulesets-stored-timestamp: {}", uc.name), new_rulesets_timestamp);
                some_updated = true;
            } else {
                info!("{}: No new ruleset bundle discovered.", uc.name);
            }
        }

        if some_updated {
            self.apply_stored_rulesets();
        }
    }

    /// Modify rulesets struct to apply the stored rulesets
    pub fn apply_stored_rulesets(&mut self) {
        type OkResult = (Value, Option<String>, bool);

        // TODO: Use futures to asynchronously apply stored rulesets
        let rulesets_closure = |uc: &UpdateChannel| -> Result<OkResult, Box<dyn Error>> {
            let rulesets_json_string = self.storage.get_string(format!("rulesets: {}", &uc.name));

            if rulesets_json_string != "" {
                info!("{}: Applying stored rulesets.", &uc.name);

                let rulesets_json_value: Value = serde_json::from_str(&rulesets_json_string)?;
                let inner_rulesets: Value = rulesets_json_value.get("rulesets").unwrap().clone();
                Ok((inner_rulesets, uc.scope.clone(), uc.replaces_default_rulesets))
            } else {
                Err(Box::new(UpdaterError::new(format!("{} Could not retrieve stored rulesets", &uc.name))))
            }
        };

        let mut rulesets_tuple_results = vec![];
        for uc in self.update_channels.get_all() {
            rulesets_tuple_results.push(rulesets_closure(uc));
        }

        let rulesets_tuples: Vec<OkResult> = rulesets_tuple_results.into_iter().filter(|rt| rt.is_ok()).map(|rt| rt.unwrap()).collect();
        let replaces = rulesets_tuples.iter().fold(false, |acc, rt| {
            if rt.2 {
                true
            } else {
                acc
            }
        });

        self.rulesets.clear();
        for rt in rulesets_tuples {
            self.rulesets.add_all_from_serde_value(rt.0, &ENABLE_MIXED_RULESETS, &RULE_ACTIVE_STATES, &rt.1);
        }

        if !replaces && !self.default_rulesets.is_none() {
            self.rulesets.add_all_from_json_string(&self.default_rulesets.clone().unwrap(), &ENABLE_MIXED_RULESETS, &RULE_ACTIVE_STATES, &None);
        }
    }

    /// Return the time until we should check for new rulesets, in seconds
    pub fn time_to_next_check(&self) -> usize {
        let last_checked = self.storage.get_int(String::from("last-checked"));
        let current_timestamp = Self::current_timestamp();
        let secs_since_last_checked = current_timestamp - last_checked;
        cmp::max(0, self.periodicity as isize - secs_since_last_checked as isize) as usize
    }
}