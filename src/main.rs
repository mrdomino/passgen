// Copyright 2025 Steven Dee
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod randexp;

use std::{
    collections::HashMap,
    env::{self},
    fs::{create_dir_all, read_to_string, write},
    io::{IsTerminal, Write, stdout},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use argon2::Argon2;
use blake3::OutputReader;
use clap::Parser;
use crypto_bigint::{NonZero, RandomMod, U256};
use rand_core::RngCore;
use randexp::{Enumerable, Expr, Quantifiable, Words};
use rpassword::prompt_password;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

fn default_schema() -> String {
    "[A-Za-z0-9]{16}".into()
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    #[serde(default = "default_schema")]
    pub default_schema: String,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    pub sites: Vec<Site>,
}

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
struct Site {
    pub name: String,
    pub schema: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub increment: u32,
}

impl Default for Config {
    fn default() -> Self {
        let mut aliases = HashMap::new();
        aliases.insert("alnum".to_string(), "[A-Za-z0-9]{18}".to_string());
        aliases.insert(
            "apple".to_string(),
            "[:Word:](-[:word:]){3}[0-9!-/]".to_string(),
        );
        aliases.insert("login".to_string(), "[!-~]{12}".to_string());
        aliases.insert("mobile".to_string(), "[a-z0-9]{24}".to_string());
        aliases.insert("phrase".to_string(), "[:word:](-[:word:]){4}".to_string());
        aliases.insert("pin".to_string(), "[0-9]{8}".to_string());
        let sites = vec![
            Site {
                name: "apple.com".to_string(),
                schema: "apple".to_string(),
                increment: 0,
            },
            Site {
                name: "google.com".to_string(),
                schema: "mobile".to_string(),
                increment: 0,
            },
            Site {
                name: "iphone.local".to_string(),
                schema: "pin".to_string(),
                increment: 1,
            },
        ];
        let default_schema = "login".to_string();
        Config {
            default_schema,
            aliases,
            sites,
        }
    }
}

impl Config {
    // TODO: toml
    pub fn from_file(path: &Path) -> Result<Self> {
        let mut config = if path.exists() {
            serde_yaml::from_str(&read_to_string(path)?)?
        } else {
            create_dir_all(path.parent().context("invalid file path")?)?;
            let default_config = Config::default();
            write(path, serde_yaml::to_string(&default_config)?)?;
            default_config
        };
        if let Some(schema) = config.aliases.get(&config.default_schema) {
            config.default_schema = schema.clone();
        }
        config.sites = config
            .sites
            .into_iter()
            .map(|site| {
                if let Some(schema) = config.aliases.get(&site.schema) {
                    Site {
                        schema: schema.clone(),
                        ..site
                    }
                } else {
                    site
                }
            })
            .collect();
        Ok(config)
    }
}

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// The site for which to generate a password
    site: String,

    /// Override the path of the config file (default: ~/.config/onepass/config.yaml)
    #[arg(
        short = 'f',
        long = "config",
        env = "ONEPASS_CONFIG_FILE",
        value_name = "CONFIG_FILE"
    )]
    config_path: Option<String>,

    /// Read words from the specified newline-separated dictionary file (by default, uses words
    /// from the EFF large word list)
    #[arg(
        short,
        long = "words",
        env = "ONEPASS_WORDS_FILE",
        value_name = "WORDS_FILE"
    )]
    words_path: Option<String>,

    /// Override schema to use for this site (may be a configured alias)
    #[arg(short, long)]
    schema: Option<String>,

    /// Override increment to use for this site
    #[arg(short, long, value_name = "NUM")]
    increment: Option<u32>,

    /// Confirm master password
    #[arg(short, long)]
    confirm: bool,

    /// Print verbose password entropy output
    #[arg(short, long)]
    verbose: bool,
}

include!(concat!(env!("OUT_DIR"), "/wordlist.rs"));

struct Blake3Rng(Zeroizing<OutputReader>);
impl RngCore for Blake3Rng {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0u8; 4];
        self.0.fill(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.0.fill(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        self.0.fill(dst);
    }

    fn try_fill_bytes(
        &mut self,
        dest: &mut [u8],
    ) -> std::result::Result<(), crypto_bigint::rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

fn default_config_path() -> Result<Box<Path>> {
    let mut config_dir = match env::var("XDG_CONFIG_DIR") {
        Err(env::VarError::NotPresent) => {
            env::var("HOME").map(|home| PathBuf::from(home).join(".config"))
        }
        r => r.map(|config| config.into()),
    }
    .context("failed finding config dir")?;
    config_dir.push("onepass");
    config_dir.push("config.yaml");
    Ok(config_dir.into_boxed_path())
}

fn main() -> Result<()> {
    let args = Args::parse();

    let config_path = args
        .config_path
        .map_or_else(default_config_path, |s| Ok(PathBuf::from(s).into()))?;
    let config = Config::from_file(&config_path).context("failed to read config")?;

    let words_string = args
        .words_path
        .map(|path| read_to_string(path).map(|s| s.into_boxed_str()))
        .transpose()
        .context("failed reading words file")?;
    let words_list = words_string
        .as_ref()
        .map(|words| words.lines().map(|line| line.trim()).collect::<Box<[_]>>());
    let words = Words::from(words_list.as_ref().map_or(EFF_WORDLIST, |x| x));

    let site = config.sites.iter().find(|&site| site.name == args.site);
    let schema = args.schema.as_ref().map_or_else(
        || site.map_or(&config.default_schema, |site| &site.schema),
        |schema| config.aliases.get(schema).unwrap_or(schema),
    );
    let increment = args
        .increment
        .unwrap_or_else(|| site.map_or(0, |site| site.increment));
    let expr = Expr::parse(schema).context("invalid schema")?;
    let size = words.size(&expr);

    if args.verbose {
        eprintln!(
            "schema has about {0} bits of entropy (0x{1} possible passwords)",
            &size.bits(),
            &size.to_string().trim_start_matches('0')
        );
    }

    let password: Zeroizing<String> = prompt_password("Master password: ")
        .context("failed reading password")?
        .into();
    if args.confirm {
        let confirmed: Zeroizing<String> = prompt_password("Confirm: ")
            .context("failed reading confirmation")?
            .into();
        if *confirmed != *password {
            anyhow::bail!("Passwords don’t match");
        }
    }
    let salt = format!("{0}:{1}", increment, &args.site);
    let mut key_material = Zeroizing::new([0u8; 32]);
    let argon2 = Argon2::new(
        argon2::Algorithm::Argon2d,
        argon2::Version::V0x13,
        argon2::Params::default(),
    );
    argon2
        .hash_password_into(password.as_bytes(), salt.as_bytes(), &mut *key_material)
        .map_err(|e| anyhow::anyhow!("argon2 failed: {e}"))?;

    let mut hasher = Zeroizing::new(blake3::Hasher::new());
    hasher.update(&*key_material);
    let mut rng = Blake3Rng(Zeroizing::new(hasher.finalize_xof()));
    let index = U256::random_mod(&mut rng, &NonZero::new(size).unwrap());
    let res = words.gen_at(&expr, index)?;
    let mut stdout = stdout();
    stdout.write_all(res.as_bytes())?;
    if stdout.is_terminal() {
        writeln!(stdout)?;
    }
    Ok(())
}
