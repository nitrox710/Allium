use std::{
    collections::{HashSet, VecDeque},
    ffi::OsStr,
    fs::{self, File},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use common::{
    constants::{ALLIUM_GAMES_DIR, ALLIUM_SD_ROOT},
    database::{Database, NewGame},
    locale::Locale,
};
use log::{error, trace};
use serde::{Deserialize, Serialize};

use crate::{
    consoles::ConsoleMapper,
    entry::{game::Game, gamelist::GameList, lazy_image::LazyImage, short_name, Entry},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Directory {
    pub name: String,
    pub full_name: String,
    pub path: PathBuf,
    /// image is loaded lazily.
    /// None means image hasn't been looked for, Some(None) means no image was found, Some(Some(path)) means an image was found.
    pub image: LazyImage,
}

impl Ord for Directory {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.full_name.cmp(&other.full_name)
    }
}

impl PartialOrd for Directory {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Default for Directory {
    fn default() -> Self {
        Directory {
            name: "Games".to_string(),
            full_name: "Games".to_string(),
            path: ALLIUM_GAMES_DIR.to_owned(),
            image: LazyImage::Unknown(ALLIUM_GAMES_DIR.to_owned()),
        }
    }
}

impl Directory {
    pub fn new(path: PathBuf) -> Directory {
        let full_name = path
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("")
            .to_string();
        let name = short_name(&full_name);
        let image = LazyImage::Unknown(path.clone());
        Directory {
            name,
            full_name,
            path,
            image,
        }
    }

    pub fn with_name(path: PathBuf, name: String) -> Directory {
        let full_name = path
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("")
            .to_string();
        let image = LazyImage::Unknown(path.clone());
        Directory {
            name,
            full_name,
            path,
            image,
        }
    }

    pub fn image(&mut self) -> Option<&Path> {
        self.image.image()
    }

    fn parse_game_list(&self, game_list: &Path) -> Result<Vec<Entry>> {
        let file = File::open(game_list)?;
        let gamelist: GameList = serde_xml_rs::from_reader(file)?;

        let games = gamelist.games.into_iter().filter_map(|game| {
            let path = self.path.join(&game.path).canonicalize().ok()?;
            if !path.exists() {
                return None;
            }

            let extension = game
                .path
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_owned();

            let full_name = game.name.clone();

            let image = game.image.or(game.thumbnail);
            let image = match image {
                Some(image) => {
                    let path = self.path.join(image).canonicalize().ok()?;
                    if path.exists() {
                        LazyImage::Found(path)
                    } else {
                        LazyImage::Unknown(self.path.clone())
                    }
                }
                None => LazyImage::Unknown(path.clone()),
            };

            Some(Entry::Game(Game {
                path,
                name: game.name,
                full_name,
                image,
                extension,
                core: None,
            }))
        });

        let folders = gamelist.folders.into_iter().filter_map(|folder| {
            let path = self.path.join(&folder.path);
            if !path.exists() {
                return None;
            }

            let name = folder.name;

            Some(Entry::Directory(Directory::with_name(path, name)))
        });

        Ok(folders.chain(games).collect())
    }

    pub fn entries(
        &self,
        database: &Database,
        console_mapper: &ConsoleMapper,
        #[allow(unused)] locale: &Locale,
    ) -> Result<Vec<Entry>> {
        let mut entries: Vec<Entry> = Vec::with_capacity(64);

        let fingerprint = database.get_gamelist_fingerprint(&self.path)?;
        let should_parse_gamelist = |path: &Path| -> Result<bool> {
            if !path.exists() {
                trace!("File doesn't exist, don't parse.");
                return Ok(false);
            }

            if let Some(fingerprint) = fingerprint {
                let Ok(metadata) = fs::metadata(path) else {
                    trace!("Failed to get gamelist metadata, don't parse.");
                    return Ok(false);
                };
                let file_size = metadata.len();
                if file_size == fingerprint {
                    trace!("Same gamelist size, not parsing.");
                    return Ok(false);
                }
                database.set_gamelist_fingerprint(&self.path, file_size)?;
                trace!("Different gamelist size, parse gamelist.");
                Ok(true)
            } else {
                trace!("No gamelist fingerprint, parse gamelist.");
                let Ok(metadata) = fs::metadata(path) else {
                    trace!("Failed to get gamelist metadata, don't parse.");
                    return Ok(false);
                };
                let file_size = metadata.len();
                database.set_gamelist_fingerprint(&self.path, file_size)?;
                Ok(true)
            }
        };

        let gamelist = self.path.join("gamelist.xml");
        if should_parse_gamelist(&gamelist)? {
            #[cfg(feature = "miyoo")]
            {
                std::process::Command::new("show")
                    .arg("--darken")
                    .spawn()?
                    .wait()?;
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "directory".to_string(),
                    self.path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into(),
                );
                std::process::Command::new("say")
                    .arg(locale.ta("populating-games", &map))
                    .spawn()?
                    .wait()?;
            }
            match self.parse_game_list(&gamelist) {
                Ok(res) => {
                    database.update_games(
                        &res.iter()
                            .filter_map(|e| match e {
                                Entry::Game(game) => Some(NewGame {
                                    name: game.name.clone(),
                                    path: game.path.clone(),
                                    image: game.image.try_image().map(Path::to_path_buf),
                                    core: game.core.clone(),
                                }),
                                Entry::App(_) | Entry::Directory(_) => None,
                            })
                            .collect::<Vec<_>>(),
                    )?;
                    entries.extend(res);
                }
                Err(e) => error!("Failed to parse gamelist.xml: {}", e),
            }
        } else if !gamelist.exists() {
            let gamelist = self.path.join("miyoogamelist.xml");
            if should_parse_gamelist(&gamelist)? {
                #[cfg(feature = "miyoo")]
                {
                    std::process::Command::new("show")
                        .arg("--darken")
                        .spawn()?
                        .wait()?;
                    let mut map = std::collections::HashMap::new();
                    map.insert(
                        "directory".to_string(),
                        self.path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into(),
                    );
                    std::process::Command::new("say")
                        .arg(locale.ta("populating-games", &map))
                        .spawn()?
                        .wait()?;
                }
                match self.parse_game_list(&gamelist) {
                    Ok(res) => {
                        database.update_games(
                            &res.iter()
                                .filter_map(|e| match e {
                                    Entry::Game(game) => Some(NewGame {
                                        name: game.name.clone(),
                                        path: game.path.clone(),
                                        image: game.image.try_image().map(Path::to_path_buf),
                                        core: game.core.clone(),
                                    }),
                                    Entry::App(_) | Entry::Directory(_) => None,
                                })
                                .collect::<Vec<_>>(),
                        )?;
                        entries.extend(res);
                    }
                    Err(e) => error!("Failed to parse gamelist.xml: {}", e),
                }
            }
        }

        entries.extend(
            database
                .select_games_in_directory(&self.path)?
                .into_iter()
                .map(Game::from_db)
                .map(Entry::Game),
        );

        entries.extend(
            std::fs::read_dir(&self.path)
                .map_err(|e| anyhow!("Failed to open directory: {:?}, {}", &self.path, e))?
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| match Entry::new(entry.path(), console_mapper) {
                    Ok(Some(entry)) => Some(entry),
                    _ => None,
                }),
        );

        let mut uniques = HashSet::new();
        entries.retain(|e| uniques.insert(e.path().to_path_buf()));

        for entry in entries.iter_mut() {
            if let Entry::Game(game) = entry {
                if let Some(core) = database.get_core(&game.path)? {
                    game.core = Some(core);
                }
            }
        }

        Ok(entries)
    }

    /// Populate the database with the games in this directory, pushing any subdirectories onto the
    /// queue.
    pub fn populate_db(
        &self,
        queue: &mut VecDeque<Directory>,
        database: &Database,
        console_mapper: &ConsoleMapper,
        locale: &Locale,
    ) -> Result<()> {
        let entries = self.entries(database, console_mapper, locale)?;

        for entry in &entries {
            match entry {
                Entry::Directory(dir) => queue.push_back(dir.clone()),
                Entry::Game(_) | Entry::App(_) => {}
            }
        }

        let games: Vec<_> = entries
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Game(game) => Some(NewGame {
                    name: game.name,
                    path: game.path,
                    image: game.image.try_image().map(Path::to_path_buf),
                    core: game.core,
                }),
                _ => None,
            })
            .collect();
        database.update_games(&games)?;

        Ok(())
    }
}

impl From<&Path> for Directory {
    fn from(path: &Path) -> Self {
        Directory::new(path.into())
    }
}
