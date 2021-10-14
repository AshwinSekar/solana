use {
    crate::consensus::{Result, Tower, TowerError, TowerVersions},
    crate::tower1_7_14::SavedTower1_7_14,
    solana_sdk::{
        pubkey::Pubkey,
        signature::{Signature, Signer},
    },
    std::{
        fs::{self, File},
        io::{self, BufReader},
        path::PathBuf,
        sync::RwLock,
    },
};

#[typetag::serde{tag = "type"}]
pub trait SavedTowerVersion {
    fn try_into_tower(&self, node_pubkey: &Pubkey) -> Result<Tower>;
    fn serialize_into(&self, file: &mut File) -> Result<()>;
    fn pubkey(&self) -> Pubkey;
}

#[frozen_abi(digest = "Gaxfwvx5MArn52mKZQgzHmDCyn5YfCuTHvp5Et3rFfpp")]
#[derive(Default, Clone, Serialize, Deserialize, Debug, PartialEq, AbiExample)]
pub struct SavedTower {
    signature: Signature,
    data: Vec<u8>,
    #[serde(skip)]
    node_pubkey: Pubkey,
}

impl SavedTower {
    pub fn new<T: Signer>(tower: &Tower, keypair: &T) -> Result<Self> {
        let node_pubkey = keypair.pubkey();
        if tower.node_pubkey != node_pubkey {
            return Err(TowerError::WrongTower(format!(
                "node_pubkey is {:?} but found tower for {:?}",
                node_pubkey, tower.node_pubkey
            )));
        }

        let data = bincode::serialize(tower)?;
        let signature = keypair.sign_message(&data);
        Ok(Self {
            signature,
            data,
            node_pubkey,
        })
    }
}

#[typetag::serde]
impl SavedTowerVersion for SavedTower {
    fn try_into_tower(&self, node_pubkey: &Pubkey) -> Result<Tower> {
        // This method assumes that `self` was just deserialized
        assert_eq!(self.node_pubkey, Pubkey::default());

        if !self.signature.verify(node_pubkey.as_ref(), &self.data) {
            return Err(TowerError::InvalidSignature);
        }
        bincode::deserialize(&self.data)
            .map(TowerVersions::Current)
            .map_err(|e| e.into())
            .and_then(|tv: TowerVersions| {
                let tower = tv.convert_to_current();
                if tower.node_pubkey != *node_pubkey {
                    return Err(TowerError::WrongTower(format!(
                        "node_pubkey is {:?} but found tower for {:?}",
                        node_pubkey, tower.node_pubkey
                    )));
                }
                Ok(tower)
            })
    }

    fn serialize_into(&self, file: &mut File) -> Result<()> {
        bincode::serialize_into(file, self).map_err(|e| e.into())
    }

    fn pubkey(&self) -> Pubkey {
        self.node_pubkey
    }
}

pub trait TowerStorage: Sync + Send {
    fn load(&self, node_pubkey: &Pubkey) -> Result<Box<dyn SavedTowerVersion>>;
    fn store(&self, saved_tower: &dyn SavedTowerVersion) -> Result<()>;
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct NullTowerStorage {}

impl TowerStorage for NullTowerStorage {
    fn load(&self, _node_pubkey: &Pubkey) -> Result<Box<dyn SavedTowerVersion>> {
        Err(TowerError::IoError(io::Error::new(
            io::ErrorKind::Other,
            "NullTowerStorage::load() not available",
        )))
    }

    fn store(&self, _saved_tower: &dyn SavedTowerVersion) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct FileTowerStorage {
    pub tower_path: PathBuf,
    migration: bool,
}

impl FileTowerStorage {
    pub fn new(tower_path: PathBuf) -> Self {
        Self {
            tower_path,
            migration: false,
        }
    }

    pub fn new_migration(tower_path: PathBuf, migration: bool) -> Self {
        Self {
            tower_path,
            migration,
        }
    }

    pub fn filename(&self, node_pubkey: &Pubkey) -> PathBuf {
        self.tower_path
            .join(format!("tower-{}", node_pubkey))
            .with_extension("bin")
    }
}

impl TowerStorage for FileTowerStorage {
    fn load(&self, node_pubkey: &Pubkey) -> Result<Box<dyn SavedTowerVersion>> {
        let filename = self.filename(node_pubkey);
        trace!("load {}", filename.display());

        // Ensure to create parent dir here, because restore() precedes save() always
        fs::create_dir_all(&filename.parent().unwrap())?;

        let file = File::open(&filename)?;
        let mut stream = BufReader::new(file);

        if self.migration {
            bincode::deserialize_from(&mut stream)
                .map_err(|e| e.into())
                .map(|t: SavedTower1_7_14| Box::new(t) as Box<dyn SavedTowerVersion>)
        } else {
            bincode::deserialize_from(&mut stream)
                .map_err(|e| e.into())
                .map(|t: SavedTower| Box::new(t) as Box<dyn SavedTowerVersion>)
        }
    }

    fn store(&self, saved_tower: &dyn SavedTowerVersion) -> Result<()> {
        let pubkey = saved_tower.pubkey();
        let filename = self.filename(&pubkey);
        trace!("store: {}", filename.display());
        let new_filename = filename.with_extension("bin.new");

        {
            // overwrite anything if exists
            let mut file = File::create(&new_filename)?;
            saved_tower.serialize_into(&mut file)?;
            // file.sync_all() hurts performance; pipeline sync-ing and submitting votes to the cluster!
        }
        fs::rename(&new_filename, &filename)?;
        // self.path.parent().sync_all() hurts performance same as the above sync
        Ok(())
    }
}

pub struct EtcdTowerStorage {
    client: RwLock<etcd_client::Client>,
    instance_id: [u8; 8],
    runtime: tokio::runtime::Runtime,
    migration: bool,
}

pub struct EtcdTlsConfig {
    pub domain_name: String,
    pub ca_certificate: Vec<u8>,
    pub identity_certificate: Vec<u8>,
    pub identity_private_key: Vec<u8>,
}

impl EtcdTowerStorage {
    pub fn new<E: AsRef<str>, S: AsRef<[E]>>(
        endpoints: S,
        tls_config: Option<EtcdTlsConfig>,
    ) -> Result<Self> {
        Self::new_migration(endpoints, tls_config, false)
    }

    pub fn new_migration<E: AsRef<str>, S: AsRef<[E]>>(
        endpoints: S,
        tls_config: Option<EtcdTlsConfig>,
        migration: bool,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();

        let client = runtime
            .block_on(async {
                etcd_client::Client::connect(
                    endpoints,
                    tls_config.map(|tls_config| {
                        etcd_client::ConnectOptions::default().with_tls(
                            etcd_client::TlsOptions::new()
                                .domain_name(tls_config.domain_name)
                                .ca_certificate(etcd_client::Certificate::from_pem(
                                    tls_config.ca_certificate,
                                ))
                                .identity(etcd_client::Identity::from_pem(
                                    tls_config.identity_certificate,
                                    tls_config.identity_private_key,
                                )),
                        )
                    }),
                )
                .await
            })
            .map_err(Self::etdc_to_tower_error)?;

        Ok(Self {
            client: RwLock::new(client),
            instance_id: solana_sdk::timing::timestamp().to_le_bytes(),
            runtime,
            migration,
        })
    }

    fn get_keys(node_pubkey: &Pubkey) -> (String, String) {
        let instance_key = format!("{}/instance", node_pubkey);
        let tower_key = format!("{}/tower", node_pubkey);
        (instance_key, tower_key)
    }

    fn etdc_to_tower_error(error: etcd_client::Error) -> TowerError {
        TowerError::IoError(io::Error::new(io::ErrorKind::Other, error.to_string()))
    }
}

impl TowerStorage for EtcdTowerStorage {
    fn load(&self, node_pubkey: &Pubkey) -> Result<Box<dyn SavedTowerVersion>> {
        let (instance_key, tower_key) = Self::get_keys(node_pubkey);
        let mut client = self.client.write().unwrap();

        let txn = etcd_client::Txn::new().and_then(vec![etcd_client::TxnOp::put(
            instance_key.clone(),
            self.instance_id,
            None,
        )]);
        self.runtime
            .block_on(async { client.txn(txn).await })
            .map_err(|err| {
                error!("Failed to acquire etcd instance lock: {}", err);
                Self::etdc_to_tower_error(err)
            })?;

        let txn = etcd_client::Txn::new()
            .when(vec![etcd_client::Compare::value(
                instance_key,
                etcd_client::CompareOp::Equal,
                self.instance_id,
            )])
            .and_then(vec![etcd_client::TxnOp::get(tower_key, None)]);

        let response = self
            .runtime
            .block_on(async { client.txn(txn).await })
            .map_err(|err| {
                error!("Failed to read etcd saved tower: {}", err);
                Self::etdc_to_tower_error(err)
            })?;

        if !response.succeeded() {
            return Err(TowerError::IoError(io::Error::new(
                io::ErrorKind::Other,
                format!("Lost etcd instance lock for {}", node_pubkey),
            )));
        }

        for op_response in response.op_responses() {
            if let etcd_client::TxnOpResponse::Get(get_response) = op_response {
                if let Some(kv) = get_response.kvs().get(0) {
                    if self.migration {
                        return bincode::deserialize_from(kv.value())
                            .map_err(|e| e.into())
                            .map(|t: SavedTower1_7_14| Box::new(t) as Box<dyn SavedTowerVersion>);
                    } else {
                        return bincode::deserialize_from(kv.value())
                            .map_err(|e| e.into())
                            .map(|t: SavedTower| Box::new(t) as Box<dyn SavedTowerVersion>);
                    }
                }
            }
        }

        // Should never happen...
        Err(TowerError::IoError(io::Error::new(
            io::ErrorKind::Other,
            "Saved tower response missing".to_string(),
        )))
    }

    // TODO: make compatible
    fn store(&self, saved_tower: &dyn SavedTowerVersion) -> Result<()> {
        let (instance_key, tower_key) = Self::get_keys(&saved_tower.pubkey());
        let mut client = self.client.write().unwrap();

        let txn = etcd_client::Txn::new()
            .when(vec![etcd_client::Compare::value(
                instance_key,
                etcd_client::CompareOp::Equal,
                self.instance_id,
            )])
            .and_then(vec![etcd_client::TxnOp::put(
                tower_key,
                bincode::serialize(saved_tower)?,
                None,
            )]);

        let response = self
            .runtime
            .block_on(async { client.txn(txn).await })
            .map_err(|err| {
                error!("Failed to write etcd saved tower: {}", err);
                err
            })
            .map_err(Self::etdc_to_tower_error)?;

        if !response.succeeded() {
            return Err(TowerError::IoError(io::Error::new(
                io::ErrorKind::Other,
                format!("Lost etcd instance lock for {}", saved_tower.pubkey()),
            )));
        }
        Ok(())
    }
}
