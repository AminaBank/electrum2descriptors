use crate::{ElectrumExtendedKey, ElectrumExtendedPrivKey, ElectrumExtendedPubKey};
use bitcoin::util::bip32::{ExtendedPrivKey, ExtendedPubKey};
use regex::Regex;
use serde::{de, ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, path::Path, str::FromStr, string::ToString};

/// Representation of an electrum wallet file
#[derive(Clone, Debug, PartialEq)]
pub struct ElectrumWalletFile {
    pub addresses: Addresses,
    pub wallet_type: WalletType,
    pub keystores: Vec<Keystore>,
}

impl ElectrumWalletFile {
    /// Parse an electrum wallet file
    pub fn from_file(wallet_file: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(wallet_file).map_err(|e| e.to_string())?;
        let wallet = serde_json::from_reader(file).map_err(|e| e.to_string())?;
        Ok(wallet)
    }

    /// Write to an electrum wallet file
    pub fn to_file(&self, wallet_file: &Path) -> Result<(), String> {
        let file = std::fs::File::create(wallet_file).map_err(|e| e.to_string())?;
        serde_json::to_writer_pretty(file, self).map_err(|e| e.to_string())
    }

    /// Convert from an output descriptor. Only the external descriptor is needed, the change descriptor is implied.
    pub fn from_descriptor(desc: &str) -> Result<Self, String> {
        if desc.contains("sortedmulti") {
            ElectrumWalletFile::from_descriptor_multisig(desc)
        } else {
            ElectrumWalletFile::from_descriptor_singlesig(desc)
        }
    }

    fn from_descriptor_singlesig(desc: &str) -> Result<Self, String> {
        let re =
            Regex::new(r#"(pkh|sh\(wpkh|sh\(wsh|wpkh|wsh)\((([tx]p(ub|rv)[0-9A-Za-z]+)/0/\*)\)+"#)
                .map_err(|e| e.to_string())?;
        let captures = re.captures(desc).map(|captures| {
            captures
                .iter()
                .skip(1)
                .take(3)
                .flatten()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
        });
        let keystore = match captures.as_deref() {
            Some([kind, _, xkey]) => Keystore::new(kind, xkey)?,
            _ => return Err(format!("Unknown descriptor format: {:?}", captures)),
        };

        let wallet = ElectrumWalletFile {
            addresses: Addresses::new(),
            keystores: vec![keystore],
            wallet_type: WalletType::Standard,
        };
        Ok(wallet)
    }

    fn from_descriptor_multisig(desc: &str) -> Result<Self, String> {
        let re = Regex::new(
            r#"(sh|sh\(wsh|wsh)\(sortedmulti\((\d),([tx]p(ub|rv)[0-9A-Za-z]+/0/\*,?)+\)+"#,
        )
        .map_err(|e| e.to_string())?;
        let captures = re.captures(desc).map(|captures| {
            captures
                .iter()
                .skip(1)
                .take(2)
                .flatten()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
        });
        if let Some([kind, x]) = captures.as_deref() {
            let kind = match *kind {
                "wsh" => "wsh",
                "sh" => "pkh",
                "sh(wsh" => "sh(wsh",
                _ => return Err(format!("unknown nultisig kind: {}", kind)),
            };
            let re = Regex::new(r#"[tx]p[ur][bv][0-9A-Za-z]+"#).map_err(|e| e.to_string())?;
            let keystores = re
                .captures_iter(desc)
                .map(|cap| Keystore::new(kind, &cap[0]))
                .collect::<Result<Vec<Keystore>, _>>()?;
            let y = keystores.len();
            if y < 2 {
                return Err(
                    "Multisig with less than two signers doesn't make a lot of sense".to_string(),
                );
            }

            let wallet = ElectrumWalletFile {
                addresses: Addresses::new(),
                keystores,
                wallet_type: WalletType::Multisig(x.parse().unwrap(), y as u8),
            };
            Ok(wallet)
        } else {
            Err(format!(
                "Unknown multisig descriptor format: {:?}",
                captures
            ))
        }
    }

    /// Generate output descriptors matching the electrum wallet
    pub fn to_descriptors(&self) -> Result<Vec<String>, String> {
        match self.wallet_type {
            WalletType::Standard => {
                let exkey = self.keystores[0].get_xkey()?;
                let desc_ext = exkey.kind().to_string() + "(" + &exkey.xkeystr() + "/0/*)";
                let desc_chg = exkey.kind().to_string() + "(" + &exkey.xkeystr() + "/1/*)";
                Ok(vec![desc_ext, desc_chg])
            }
            WalletType::Multisig(x, _y) => {
                let xkeys = self
                    .keystores
                    .iter()
                    .map(|ks| ks.get_xkey())
                    .collect::<Result<Vec<Box<dyn ElectrumExtendedKey>>, _>>()?;
                let prefix = match &xkeys[0].kind().to_string() as &str {
                    "pkh" => "sh",
                    kind => kind,
                }
                .to_string();
                let prefix = format!("{}(sortedmulti({}", prefix, x);

                let mut desc = xkeys.iter().fold(prefix, |acc, exkey| {
                    acc + &(",".to_string() + &exkey.xkeystr() + "/0/*")
                });
                desc += "))";
                let opening = desc.matches('(').count();
                let closing = desc.matches(')').count();
                if opening > closing {
                    desc += ")"
                };
                let desc_chg = desc.replace("/0/*", "/1/*");

                Ok(vec![desc, desc_chg])
            }
        }
    }
}

impl Serialize for ElectrumWalletFile {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // We don't know the length of the map at this point, so it's None
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("addresses", &self.addresses)?;
        map.serialize_entry("wallet_type", &self.wallet_type)?;
        match self.wallet_type {
            WalletType::Standard => {
                map.serialize_entry("keystore", &self.keystores[0])?;
            }
            WalletType::Multisig(_x, _y) => {
                self.keystores
                    .iter()
                    .enumerate()
                    .map(|(i, keystore)| {
                        let key = format!("x{}/", i + 1);
                        map.serialize_entry(&key, &keystore)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ElectrumWalletFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        enum Field {
            Addrs,
            Keyst,
            WalTyp,
            AddrHistory,
            WinPosQt,
            IgnoreBool,
            IgnoreString,
            IgnoreNumber,
            IgnoreMap,
            IgnoreVec,
        }

        impl<'de> Deserialize<'de> for Field {
            fn deserialize<D>(deserializer: D) -> Result<Field, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct FieldVisitor;

                impl<'de> de::Visitor<'de> for FieldVisitor {
                    type Value = Field;

                    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                        formatter.write_str(
                            "`addresses` or `keystore` or `wallet_type` or 'x1/` or `x2/`",
                        )
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Field, E>
                    where
                        E: de::Error,
                    {
                        let re = Regex::new(r#"(x)(\d+)(/)|([a-z_\-0-9]+)"#).unwrap();
                        let captures = re.captures(value).map(|captures| {
                            captures
                                .iter()
                                .skip(1)
                                .flatten()
                                .map(|c| c.as_str())
                                .collect::<Vec<_>>()
                        });
                        match captures.as_deref() {
                            Some(["x", _i, "/"]) => Ok(Field::Keyst),
                            Some(["keystore"]) => Ok(Field::Keyst),
                            Some(["addr_history"]) => Ok(Field::AddrHistory),
                            Some(["addresses"]) => Ok(Field::Addrs),
                            Some(["channel_backups"]) => Ok(Field::IgnoreMap),
                            Some(["channels"]) => Ok(Field::IgnoreMap),
                            Some(["fiat_value"]) => Ok(Field::IgnoreMap),
                            Some(["invoices"]) => Ok(Field::IgnoreMap),
                            Some(["labels"]) => Ok(Field::IgnoreMap),
                            Some(["lightning_payments"]) => Ok(Field::IgnoreMap),
                            Some(["lightning_preimages"]) => Ok(Field::IgnoreMap),
                            Some(["lightning_privkey2"]) => Ok(Field::IgnoreString),
                            Some(["payment_requests"]) => Ok(Field::IgnoreMap),
                            Some(["prevouts_by_scripthash"]) => Ok(Field::IgnoreMap),
                            Some(["qt-console-history"]) => Ok(Field::IgnoreVec),
                            Some(["seed_type"]) => Ok(Field::IgnoreString),
                            Some(["seed_version"]) => Ok(Field::IgnoreNumber),
                            Some(["spent_outpoints"]) => Ok(Field::IgnoreMap),
                            Some(["stored_height"]) => Ok(Field::IgnoreNumber),
                            Some(["submarine_swaps"]) => Ok(Field::IgnoreMap),
                            Some(["transactions"]) => Ok(Field::IgnoreMap),
                            Some(["wallet_type"]) => Ok(Field::WalTyp),
                            Some(["tx_fees"]) => Ok(Field::IgnoreMap),
                            Some(["txi"]) => Ok(Field::IgnoreMap),
                            Some(["txo"]) => Ok(Field::IgnoreMap),
                            Some(["use_change"]) => Ok(Field::IgnoreBool),
                            Some(["use_encryption"]) => Ok(Field::IgnoreBool),
                            Some(["winpos-qt"]) => Ok(Field::WinPosQt),
                            Some(["verified_tx3"]) => Ok(Field::IgnoreMap),
                            _ => Err(de::Error::unknown_field(value, FIELDS)),
                        }
                    }
                }

                deserializer.deserialize_identifier(FieldVisitor)
            }
        }

        struct ElectrumWalletFileVisitor;

        impl<'de> de::Visitor<'de> for ElectrumWalletFileVisitor {
            type Value = ElectrumWalletFile;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct ElectrumWalletFile")
            }

            fn visit_map<V>(self, mut map: V) -> Result<ElectrumWalletFile, V::Error>
            where
                V: de::MapAccess<'de>,
            {
                let mut addresses = Addresses::new();
                let mut keystores = Vec::new();
                let mut wallet_type = WalletType::Standard;

                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Addrs => {
                            addresses = map.next_value()?;
                        }
                        Field::Keyst => {
                            keystores.push(map.next_value()?);
                        }
                        Field::WalTyp => {
                            wallet_type = map.next_value()?;
                        }
                        Field::AddrHistory => {
                            let _ignore: std::collections::hash_map::HashMap<
                                String,
                                Vec<(String, usize)>,
                            > = map.next_value()?;
                        }
                        Field::WinPosQt => {
                            let _ignore: (u16, u16, u16, u16) = map.next_value()?;
                        }
                        Field::IgnoreBool => {
                            let _ignore: bool = map.next_value()?;
                        }
                        Field::IgnoreString => {
                            let _ignore: String = map.next_value()?;
                        }
                        Field::IgnoreNumber => {
                            let _ignore: usize = map.next_value()?;
                        }
                        Field::IgnoreVec => {
                            let _ignore: Vec<String> = map.next_value()?;
                        }
                        Field::IgnoreMap => {
                            let _ignore: std::collections::hash_map::HashMap<String, String> =
                                map.next_value()?;
                        }
                    }
                }

                Ok(ElectrumWalletFile {
                    addresses,
                    keystores,
                    wallet_type,
                })
            }
        }

        const FIELDS: &[&str] = &[
            "addresses",
            "addr_history",
            "channel_backups",
            "keystore",
            "wallet_type",
            "x1/",
            "x2/",
            "x3/",
        ];
        deserializer.deserialize_struct("ElectrumWalletFile", FIELDS, ElectrumWalletFileVisitor)
    }
}

/// Representation of the addresses section of an electrum wallet file
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Addresses {
    pub change: Vec<String>,
    pub receiving: Vec<String>,
}

impl Addresses {
    fn new() -> Self {
        Addresses {
            change: Vec::new(),
            receiving: Vec::new(),
        }
    }
}

/// Representation of a keystore section of an electrum wallet file. Can be single sig "keystore" or multisig "x1/" "x2/" ...
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Keystore {
    #[serde(default = "Keystore::default_type")]
    pub r#type: String,
    pub xprv: Option<String>,
    pub xpub: String,
}

impl Keystore {
    /// Construct a Keystore from script kind and xpub or xprv
    fn new(kind: &str, xkey: &str) -> Result<Self, String> {
        let xprv = ExtendedPrivKey::from_str(xkey);
        let exprv = if let Ok(xprv) = xprv {
            Some(ElectrumExtendedPrivKey::new(xprv, kind.to_string()).electrum_xprv()?)
        } else {
            None
        };

        let expub = if let Ok(xprv) = xprv {
            let secp = bitcoin::secp256k1::Secp256k1::new();
            ElectrumExtendedPubKey::new(
                ExtendedPubKey::from_private(&secp, &xprv),
                kind.to_string(),
            )
        } else {
            ElectrumExtendedPubKey::new(
                ExtendedPubKey::from_str(xkey).map_err(|e| e.to_string())?,
                kind.to_string(),
            )
        }
        .electrum_xpub()?;

        Ok(Keystore {
            r#type: Keystore::default_type(),
            xprv: exprv,
            xpub: expub,
        })
    }

    /// Get the xprv if available or else the xpub.
    fn get_xkey(&self) -> Result<Box<dyn ElectrumExtendedKey>, String> {
        if let Some(xprv) = &self.xprv {
            let exprv = ElectrumExtendedPrivKey::from_str(xprv)?;
            return Ok(Box::new(exprv));
        }

        let expub = ElectrumExtendedPubKey::from_str(&self.xpub)?;
        Ok(Box::new(expub))
    }

    /// Default keystore type to use if nothing else was specified
    fn default_type() -> String {
        "bip32".to_string()
    }
}

/// Representation of the wallet_type section of an electrum wallet file
#[derive(Clone, Debug, PartialEq)]
pub enum WalletType {
    Standard,
    Multisig(u8, u8),
}

impl fmt::Display for WalletType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl FromStr for WalletType {
    type Err = String;

    /// Parse WalletType from a string representation
    fn from_str(wallet_type: &str) -> Result<Self, Self::Err> {
        let re = Regex::new(r#"(standard)|(\d+)(of)(\d+)"#).map_err(|e| e.to_string())?;
        let captures = re.captures(wallet_type).map(|captures| {
            captures
                .iter()
                .skip(1)
                .flatten()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
        });
        match captures.as_deref() {
            Some(["standard"]) => Ok(WalletType::Standard),
            Some([x, "of", y]) => Ok(WalletType::Multisig(x.parse().unwrap(), y.parse().unwrap())),
            _ => Err(format!("Unknown wallet type: {}", wallet_type)),
        }
    }
}

impl<'de> Deserialize<'de> for WalletType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        WalletType::from_str(&s).map_err(de::Error::custom)
    }
}

impl Serialize for WalletType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = match *self {
            WalletType::Standard => "standard".to_string(),
            WalletType::Multisig(x, y) => format!("{}of{}", x, y),
        };
        serializer.serialize_str(&s)
    }
}
