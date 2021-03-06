use std::collections::{HashMap, BTreeMap};
use std::convert::TryFrom;

use tantivy::schema::{Schema, Field, TextOptions, IntOptions};
use tantivy::{Index, IndexReader, IndexWriter, Document};
use tantivy::query::QueryParser;
use tantivy::collector::TopDocs;
use tantivy::schema::Value as SchemaValue;


use crate::prelude::*;
use crate::prelude::join;
use serde_value::Value;
use serde::{Serialize};
use serde::de::DeserializeOwned;

/// Builder struct for Surfer
#[derive(Clone)]
pub struct SurferBuilder {
    schemas: HashMap<String, Schema>,
    home: Option<String>,
}


#[derive(Serialize)]
struct SingleValuedNamedFieldDocument<'a>(BTreeMap<&'a str, &'a SchemaValue>);

/// Default impl to get things going
impl Default for SurferBuilder {
    fn default() -> Self {
        let schemas = HashMap::new();
        let home = None;
        Self {
            schemas,
            home,
        }
    }
}


/// Provides access to Surfer
impl SurferBuilder {
    /// Set home location - default is indexes
    pub fn set_home(&mut self, home: &str) {
        self.home = Some(home.to_string());
    }
    /// Add a schema
    pub fn add_schema(&mut self, name: String, schema: Schema) {
        self.schemas.insert(name, schema);
    }
    /// Add serde value panics otherwise
    pub fn add_serde(&mut self, name: String, data: &Value) {
        let schema = to_schema(data, None).unwrap();
        self.schemas.insert(name, schema);
    }
    /// Add a serializable rust struct panics otherwise
    pub fn add_struct<T: Serialize>(&mut self, name: String, data: &T) {
        let value = as_value(data).unwrap();
        self.add_serde(name, &value);
    }
}

/// Surfer: Client API
pub struct Surfer {
    home: String,
    indexes: HashMap<String, Index>,
    fields: HashMap<String, Vec<Field>>,
    readers: HashMap<String, Option<IndexReader>>,
    writers: HashMap<String, Option<IndexWriter>>,
}

impl Surfer {
    /// Location of home
    pub fn home(&self) -> &String {
        &self.home
    }
    /// Location of Index
    pub fn which_index(&self, name: &str) -> Option<String> {
        if !self.indexes.contains_key(name) {
            return None;
        }
        if name.starts_with(&self.home) {
            Some(name.to_string())
        } else {
            join(&self.home, name)
        }
    }
    /// Inserts a struct
    pub fn insert_struct<T: Serialize>(&mut self, name: &str, data: &T) -> Result<(), IndexError> {
        let data = serde_json::to_string(data)?;
        let writer = self.writers.get(name);
        if writer.is_none() {
            return Ok(());
        };

        let index = self.indexes.get(name).unwrap();
        let schema = &index.schema();

        let writer = writer.unwrap();
        if writer.is_none() {
            let writer = open_index_writer(index)?;
            self.writers.insert(name.to_string(), Some(writer));
        };

        let writer = self.writers.get_mut(name).unwrap().as_mut().unwrap();
        let document = schema.parse_document(&data)?;
        writer.add_document(document);
        writer.commit()?;
        Ok(())
    }
    /// Inserts a structs
    pub fn insert_structs<T: Serialize>(&mut self, name: &str, payload: &Vec<T>) -> Result<(), IndexError> {
        let writer = self.writers.get(name);
        if writer.is_none() {
            return Ok(());
        };

        let index = self.indexes.get(name).unwrap();
        let schema = &index.schema();

        let writer = writer.unwrap();
        if writer.is_none() {
            let writer = open_index_writer(index)?;
            self.writers.insert(name.to_string(), Some(writer));
        };

        let writer = self.writers.get_mut(name).unwrap().as_mut().unwrap();
        for data in payload {
            let data = serde_json::to_string(data)?;
            let document = schema.parse_document(&data)?;
            writer.add_document(document);
        }

        writer.commit()?;
        Ok(())
    }
    /// Massive hack look away ;)
    fn jsonify(&self, name: &str, document: &Document) -> Result<String, IndexError> {
        let schema = self.indexes.get(name).unwrap().schema();

        let mut field_map = BTreeMap::new();
        for (field, field_values) in document.get_sorted_field_values() {
            let field_name = schema.get_field_name(field);
            let fv = field_values.get(0);
            if fv.is_none() {
                let message = format!("Unable to jsonify: {}", name);
                let reason = format!("Field: {} does not have any value", field_name);
                let error = IndexError::new(message, reason);
                return Err(error);
            };
            let fv = fv.unwrap().value();
            field_map.insert(field_name, fv);
        };
        let payload = SingleValuedNamedFieldDocument(field_map);
        let result = serde_json::to_string(&payload)
            .map_err(|e| {
                let message = "Unable to serialize struct".to_string();
                let reason = e.to_string();
                IndexError::new(
                    message,
                    reason,
                )
            });
        result
    }
    /// Reads as string
    pub fn read_string(&mut self, name: &str, query: &str, limit: Option<usize>, score: Option<f32>) -> Result<Option<Vec<String>>, IndexError> {
        let reader = self.readers.get(name);
        if reader.is_none() {
            return Ok(None);
        };

        let reader = reader.unwrap();
        let index = self.indexes.get(name);
        if index.is_none() {
            return Ok(None);
        };
        let index = index.unwrap();
        let reader = if reader.is_none() {
            let reader = open_index_reader(index)?;
            self.readers.insert(name.to_string(), Some(reader));
            let reader = self.readers.get(name);
            reader.unwrap().as_ref().unwrap()
        } else {
            let reader = self.readers.get(name);
            reader.unwrap().as_ref().unwrap()
        };

        let default_fields = self.fields.get(name).unwrap().clone();
        let searcher = reader.searcher();

        let query_parser = QueryParser::for_index(&index, default_fields);
        let query = query_parser.parse_query(query)?;
        let limit = if limit.is_some() {
            limit.unwrap()
        } else {
            10
        };
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        let mut docs = Vec::with_capacity(top_docs.len());
        for (doc_score, doc_address) in top_docs {
            if score.is_some() && doc_score < score.unwrap() {
                continue;
            }
            let doc = searcher.doc(doc_address)?;
            let doc = self.jsonify(name, &doc)?;
            docs.push(doc);
        };
        Ok(Some(docs))
    }
    /// Reads as struct
    pub fn read_structs<T: Serialize + DeserializeOwned>(&mut self, name: &str, query: &str, limit: Option<usize>, score: Option<f32>) -> Result<Option<Vec<T>>, IndexError> {
        let reader = self.readers.get(name);
        if reader.is_none() {
            return Ok(None);
        };

        let reader = reader.unwrap();
        let index = self.indexes.get(name);
        if index.is_none() {
            return Ok(None);
        };
        let index = index.unwrap();
        let reader = if reader.is_none() {
            let reader = open_index_reader(index)?;
            self.readers.insert(name.to_string(), Some(reader));
            let reader = self.readers.get(name);
            reader.unwrap().as_ref().unwrap()
        } else {
            let reader = self.readers.get(name);
            reader.unwrap().as_ref().unwrap()
        };

        let default_fields = self.fields.get(name).unwrap().clone();
        let searcher = reader.searcher();

        let query_parser = QueryParser::for_index(&index, default_fields);
        let query = query_parser.parse_query(query)?;
        let limit = if limit.is_some() {
            limit.unwrap()
        } else {
            10
        };
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        let mut docs = Vec::with_capacity(top_docs.len());
        for (doc_score, doc_address) in top_docs {
            if score.is_some() && doc_score < score.unwrap() {
                continue;
            }
            let doc = searcher.doc(doc_address)?;
            let doc = self.jsonify(name, &doc)?;
            let doc = serde_json::from_str::<T>(&doc).unwrap();
            docs.push(doc);
        };
        Ok(Some(docs))
    }
}

/// Panics if somethings goes wrong
impl Surfer {
    pub fn new(builder: SurferBuilder) -> Self {
        Surfer::try_from(builder).unwrap()
    }
}

/// Opens mmap dir
fn initialize_mmap(name: &str, home: &str, schema: &Schema) -> Result<Index, IndexError> {
    let path = resolve_index_directory_path(name, Some(home))?;
    if path.exists() {
        let dir = open_mmap_directory(path)?;
        open_index(dir, None)
    } else {
        let dir = open_mmap_directory(path)?;
        open_index(dir, Some(&schema))
    }
}

/// Get home location
fn extract_home(builder: &SurferBuilder) -> Result<String, IndexError> {
    let home = builder.home.as_ref();
    let home = resolve_home(home)?;
    Ok(home.to_str().unwrap().to_string())
}

/// Setup indexes
fn initialized_index(home: &str, builder: &SurferBuilder) -> Result<HashMap<String, Index>, IndexError> {
    let schemas = &builder.schemas;
    let mut indexes = HashMap::<String, Index>::with_capacity(schemas.len());
    for (name, schema) in schemas {
        let index = initialize_mmap(name, &home, &schema)?;
        indexes.insert(name.to_string(), index);
    };
    Ok(indexes)
}

/// Extract field information
fn extract_fields(builder: &SurferBuilder) -> HashMap<String, Vec<Field>> {
    let data = &builder.schemas;
    let mut fields = HashMap::<String, Vec<Field>>::with_capacity(data.len());
    for (data, schema) in data {
        let key = data.clone();
        let value: Vec<Field> = schema.fields().map(|(f, _)| f).collect();
        fields.insert(key, value);
    };
    fields
}


impl TryFrom<SurferBuilder> for Surfer {
    type Error = IndexError;
    fn try_from(builder: SurferBuilder) -> Result<Self, Self::Error> {
        let home = extract_home(&builder)?;
        let indexes = initialized_index(&home, &builder)?;
        let fields = extract_fields(&builder);

        let mut readers = HashMap::new();
        let mut writers = HashMap::new();
        for (name, _) in &builder.schemas {
            let reader: Option<IndexReader> = None;
            let writer: Option<IndexWriter> = None;
            writers.insert(name.to_string(), writer);
            readers.insert(name.to_string(), reader);
        }

        Ok(Surfer {
            home,
            indexes,
            fields,
            readers,
            writers,
        })
    }
}

/// Container to pass through config to tantivy
pub enum Control {
    ControlTextOptions(TextOptions),
    ControlIntOptions(IntOptions),
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Serialize, Deserialize};
    use std::fmt::Debug;
    use std::path::Path;
    use std::fs::remove_dir_all;


    #[derive(Clone, Serialize, Debug, Deserialize, PartialEq)]
    struct OldMan {
        title: String,
        body: String,
    }

    impl Default for OldMan {
        fn default() -> Self {
            let title = "".to_string();
            let body = "".to_string();
            Self {
                title,
                body,
            }
        }
    }

    #[test]
    fn validate_read_existing_documents_as_structs() {
        let name = random_string(None);
        let home = "tmp";
        let index_path = format!("{}/{}", home, name);
        let path = Path::new(&index_path);
        assert!(!path.exists());

        let data = OldMan::default();

        let mut builder = SurferBuilder::default();
        builder.set_home(home);
        builder.add_struct(name.clone(), &data);

        {
            let title = "The Old Man and the Sea".to_string();
            let body = "He was an old man who fished alone in a skiff in the Gulf Stream and he had gone eighty-four days now without taking a fish.".to_string();
            let old_man_doc = OldMan {
                title,
                body,
            };

            let mut surfer = Surfer::new(builder.clone());
            let _ = surfer.insert_struct(&name, &old_man_doc).unwrap();
        }

        let mut surfer = Surfer::new(builder.clone());
        let query = "sea whale";
        let result = surfer.read_structs::<OldMan>(&name, query, None, None);
        assert!(result.is_ok());
        assert!(path.exists());
        let _ = remove_dir_all(index_path);
    }

    #[test]
    fn validate_read_existing_documents_as_strings() {
        let title = "The Old Man and the Sea".to_string();
        let body = "He was an old man who fished alone in a skiff in the Gulf Stream and he had gone eighty-four days now without taking a fish.".to_string();
        let expected = OldMan {
            title,
            body,
        };


        let name = random_string(None);
        let mut builder = SurferBuilder::default();
        let data = OldMan::default();
        let home = "tmp";
        let index_path = format!("{}/{}", home, name);
        let path = Path::new(&index_path);
        assert!(!path.exists());
        builder.set_home(home);
        builder.add_struct(name.to_string(), &data);

        {
            let title = "The Old Man and the Sea".to_string();
            let body = "He was an old man who fished alone in a skiff in the Gulf Stream and he had gone eighty-four days now without taking a fish.".to_string();
            let old_man_doc = OldMan {
                title,
                body,
            };

            let mut surfer = Surfer::new(builder.clone());
            let _ = surfer.insert_struct(&name, &old_man_doc).unwrap();
        }

        let mut surfer = Surfer::new(builder.clone());
        let query = "sea whale";
        let result = surfer.read_string("Non-existent", query, None, None);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_none());
        let result = surfer.read_string(&name, query, None, None);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_some());
        let result = result.unwrap();
        let mut computed = Vec::new();
        for entry in result {
            let data: serde_json::Result<OldMan> = serde_json::from_str(&entry);
            let data = data.unwrap();
            computed.push(data);
        };
        assert_eq!(computed, vec![expected.clone()]);

        // Reading documents again
        let result = surfer.read_string(&name, query, None, None);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_some());
        let result = result.unwrap();
        let mut computed = Vec::new();
        for entry in result {
            let data: serde_json::Result<OldMan> = serde_json::from_str(&entry);
            let data = data.unwrap();
            computed.push(data);
        };
        assert_eq!(computed, vec![expected.clone()]);

        let _ = remove_dir_all(&index_path);
    }

    #[test]
    fn validate_as_rust_structs() {
        let name = random_string(None);
        let home = "tmp".to_string();
        let index_path = format!("{}/{}", home, name);
        let path = Path::new(&index_path);
        assert!(!path.exists());

        let title = "The Old Man and the Sea".to_string();
        let body = "He was an old man who fished alone in a skiff in the Gulf Stream and he had gone eighty-four days now without taking a fish.".to_string();
        let old_man_doc = OldMan {
            title,
            body,
        };


        let mut builder = SurferBuilder::default();
        builder.set_home(home.as_str());
        builder.add_struct(name.to_string(), &old_man_doc);
        let mut surfer = Surfer::new(builder);

        let _ = surfer.insert_struct(&name, &old_man_doc).unwrap();
        let query = "sea whale";

        let result = surfer.read_structs::<OldMan>("non-existent", query, None, None);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_none());

        let result = surfer.read_structs::<OldMan>(&name, query, None, None).unwrap().unwrap();
        for computed in result {
            assert_eq!(computed, old_man_doc);
        };
        assert!(path.exists());

        // Reading documents again

        let result = surfer.read_structs::<OldMan>(&name, query, None, None).unwrap().unwrap();
        for computed in result {
            assert_eq!(computed, old_man_doc);
        };


        let _ = remove_dir_all(index_path);
    }

    #[test]
    fn validate_initialize_mmap() {
        let home = "tmp/indexes";
        let index_name = "someindex";
        let path_to_index = "tmp/indexes/someindex";
        let path = Path::new(path_to_index);
        assert!(!path.exists());
        let oldman = OldMan::default();
        let data = as_value(&oldman).unwrap();
        let schema = to_schema(&data, None).unwrap();
        let _ = initialize_mmap(index_name, home, &schema);
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(path_to_index);
    }

    #[test]
    fn validate_read_existing_documents_as_structs_limit_one() {
        let name = random_string(None);
        let home = "tmp";
        let index_path = format!("{}/{}", home, name);
        let path = Path::new(&index_path);
        assert!(!path.exists());

        let data = OldMan::default();

        let mut builder = SurferBuilder::default();
        builder.set_home(home);
        builder.add_struct(name.clone(), &data);

        let title = "The Old Man and the Sea".to_string();
        let body = "He was an old man who fished alone in a skiff in the Gulf Stream and he had gone eighty-four days now without taking a fish.".to_string();
        let old_man_doc = OldMan {
            title,
            body,
        };

        let mut surfer = Surfer::new(builder.clone());
        let _ = surfer.insert_struct(&name, &old_man_doc).unwrap();
        let _ = surfer.insert_struct(&name, &old_man_doc).unwrap();
        let _ = surfer.insert_struct(&name, &old_man_doc).unwrap();
        let _ = surfer.insert_struct(&name, &old_man_doc).unwrap();
        let _ = surfer.insert_struct(&name, &old_man_doc).unwrap();

        let query = "sea whale";
        let result = surfer.read_structs::<OldMan>(&name, query, None, None);
        assert!(result.is_ok());
        let result = result.unwrap().unwrap();
        assert_eq!(result.len(), 5);

        let result = surfer.read_structs::<OldMan>(&name, query, Some(1), None);
        assert!(result.is_ok());
        let result = result.unwrap().unwrap();
        assert_eq!(result.len(), 1);

        assert!(path.exists());
        let _ = remove_dir_all(index_path);
    }

    #[test]
    fn validate_read_existing_documents_as_structs_default_ten() {
        let name = random_string(None);
        let home = "tmp";
        let index_path = format!("{}/{}", home, name);
        let path = Path::new(&index_path);
        assert!(!path.exists());

        let data = OldMan::default();

        let mut builder = SurferBuilder::default();
        builder.set_home(home);
        builder.add_struct(name.clone(), &data);

        let title = "The Old Man and the Sea".to_string();
        let body = "He was an old man who fished alone in a skiff in the Gulf Stream and he had gone eighty-four days now without taking a fish.".to_string();
        let old_man_doc = OldMan {
            title,
            body,
        };

        let mut surfer = Surfer::new(builder.clone());
        for _ in 0..20 {
            let _ = surfer.insert_struct(&name, &old_man_doc).unwrap();
        }


        let query = "sea whale";
        let result = surfer.read_structs::<OldMan>(&name, query, None, None);
        assert!(result.is_ok());
        let result = result.unwrap().unwrap();
        assert_eq!(result.len(), 10);

        let result = surfer.read_structs::<OldMan>(&name, query, Some(20), None);
        assert!(result.is_ok());
        let result = result.unwrap().unwrap();
        assert_eq!(result.len(), 20);

        assert!(path.exists());
        let _ = remove_dir_all(index_path);
    }
}