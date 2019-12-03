use crate::components::store::{Store, SubgraphDeploymentStore};
use crate::data::graphql::scalar::BuiltInScalarType;
use crate::data::graphql::traversal;
use crate::data::subgraph::{SubgraphDeploymentId, SubgraphName};
use crate::prelude::Fail;

use failure::Error;
use graphql_parser;
use graphql_parser::{
    query::{Name, Value},
    schema::{self, InterfaceType, ObjectType, TypeDefinition, *},
    Pos,
};
use serde::{Deserialize, Serialize};

use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::iter::FromIterator;
use std::sync::Arc;

pub const SCHEMA_TYPE_NAME: &str = "_schema_";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Strings(Vec<String>);

impl fmt::Display for Strings {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        let s = (&self.0).join(", ");
        write!(f, "{}", s)
    }
}

#[derive(Debug, Fail, PartialEq, Eq)]
pub enum SchemaValidationError {
    #[fail(display = "Interface {} not defined", _0)]
    UndefinedInterface(String),

    #[fail(display = "@entity directive missing on the following type: {}", _0)]
    EntityDirectivesMissing(Strings),

    #[fail(
        display = "Entity type `{}` cannot implement `{}` because it is missing \
                   the required fields: {}",
        _0, _1, _2
    )]
    CannotImplement(String, String, Strings), // (type, interface, missing_fields)
    #[fail(
        display = "Field `{}` in type `{}` has invalid @derivedFrom: {}",
        _1, _0, _2
    )]
    DerivedFromInvalid(String, String, String), // (type, field, reason)
    #[fail(display = "_SubgraphSchema_ type is solely for imports and should have no fields")]
    SubgraphSchemaTypeFieldsInvalid,
    #[fail(display = "_SubgraphSchema_ type only allows @import directives")]
    SubgraphSchemaDirectivesInvalid,
    #[fail(display = "@import defined incorrectly")]
    ImportDirectiveInvalid,
    #[fail(
        display = "GraphQL type `{}` has field `{}` with type `{}` which is not defined or imported",
        _0, _1, _2
    )]
    GraphQLTypeFieldInvalid(String, String, String), // (type_name, field_name, field_type)
}

#[derive(Debug, Fail, PartialEq, Eq, Clone)]
pub enum SchemaImportError {
    #[fail(display = "Schema for imported subgraph `{}` was not found", _0)]
    ImportedSchemaNotFound(SchemaReference),
    #[fail(display = "Subgraph for imported schema `{}` is not deployed", _0)]
    ImportedSubgraphNotFound(SchemaReference),
    #[fail(display = "Name for imported subgraph `{}` is invalid", _0)]
    ImportedSubgraphNameInvalid(String),
    #[fail(display = "Id for imported subgraph `{}` is invalid", _0)]
    ImportedSubgraphIdInvalid(String),
}

impl SchemaImportError {
    pub fn is_failure(error: &Self) -> bool {
        match error {
            SchemaImportError::ImportedSubgraphNameInvalid(_)
            | SchemaImportError::ImportedSubgraphIdInvalid(_) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportedType {
    Name(String),
    NameAs(String, String),
}

impl Hash for ImportedType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Name(name) => name.hash(state),
            Self::NameAs(name, az) => {
                name.hash(state);
                az.hash(state);
            }
        };
    }
}

impl fmt::Display for ImportedType {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            Self::Name(name) => write!(f, "{}", name),
            Self::NameAs(name, az) => write!(f, "name: {}, as: {}", name, az),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaReference {
    ByName(String),
    ById(String),
}

impl Hash for SchemaReference {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::ById(id) => id.hash(state),
            Self::ByName(name) => name.hash(state),
        };
    }
}

impl fmt::Display for SchemaReference {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            SchemaReference::ByName(name) => write!(f, "{}", name),
            SchemaReference::ById(id) => write!(f, "{}", id),
        }
    }
}

impl SchemaReference {
    pub fn resolve<S: Store + SubgraphDeploymentStore>(
        self,
        store: Arc<S>,
    ) -> Result<(Arc<Schema>, SubgraphDeploymentId), SchemaImportError> {
        let subgraph_id = match &self {
            SchemaReference::ByName(name) => {
                let subgraph_name = SubgraphName::new(name.clone())
                    .map_err(|_| SchemaImportError::ImportedSubgraphNameInvalid(name.clone()))?;
                store
                    .resolve_subgraph_name_to_id(subgraph_name.clone())
                    .map_err(|_| SchemaImportError::ImportedSubgraphNotFound(self.clone()))
                    .and_then(|subgraph_id_opt| {
                        subgraph_id_opt
                            .ok_or(SchemaImportError::ImportedSubgraphNotFound(self.clone()))
                    })?
            }
            SchemaReference::ById(id) => SubgraphDeploymentId::new(id.clone())
                .map_err(|_| SchemaImportError::ImportedSubgraphIdInvalid(id.clone()))?,
        };

        store
            .input_schema(&subgraph_id)
            .map_err(|_| SchemaImportError::ImportedSchemaNotFound(self.clone()))
            .map(|schema| (schema, subgraph_id))
    }
}

/// A validated and preprocessed GraphQL schema for a subgraph.
#[derive(Clone, Debug, PartialEq)]
pub struct Schema {
    pub id: SubgraphDeploymentId,
    pub document: schema::Document,

    // Maps type name to implemented interfaces.
    pub interfaces_for_type: BTreeMap<Name, Vec<InterfaceType>>,

    // Maps an interface name to the list of entities that implement it.
    pub types_for_interface: BTreeMap<Name, Vec<ObjectType>>,
}

impl Schema {
    /// Create a new schema. The document must already have been
    /// validated. This function is only useful for creating an introspection
    /// schema, and should not be used otherwise
    pub fn new(id: SubgraphDeploymentId, document: schema::Document) -> Self {
        Schema {
            id,
            document,
            interfaces_for_type: BTreeMap::new(),
            types_for_interface: BTreeMap::new(),
        }
    }

    pub fn resolve_schema_references<S: Store + SubgraphDeploymentStore>(
        &self,
        store: Arc<S>,
    ) -> (
        HashMap<SchemaReference, Arc<Schema>>,
        Vec<SchemaImportError>,
    ) {
        let mut schemas = HashMap::new();
        let mut visit_log = HashMap::new();
        let import_errors = self.resolve_import_graph(store, &mut schemas, &mut visit_log);
        (schemas, import_errors)
    }

    fn resolve_import_graph<S: Store + SubgraphDeploymentStore>(
        &self,
        store: Arc<S>,
        schemas: &mut HashMap<SchemaReference, Arc<Schema>>,
        visit_log: &mut HashMap<SubgraphDeploymentId, Arc<Schema>>,
    ) -> Vec<SchemaImportError> {
        // Use the visit log to detect cycles in the import graph
        self.imported_schemas()
            .into_iter()
            .fold(vec![], |mut errors, schema_ref| {
                match schema_ref.clone().resolve(store.clone()) {
                    Ok((schema, subgraph_id)) => {
                        schemas.insert(schema_ref, schema.clone());
                        // If this node in the graph has already been visited stop traversing
                        if !visit_log.contains_key(&subgraph_id) {
                            visit_log.insert(subgraph_id, schema.clone());
                            errors.extend(schema.resolve_import_graph(
                                store.clone(),
                                schemas,
                                visit_log,
                            ));
                        }
                    }
                    Err(err) => {
                        errors.push(err);
                    }
                }
                errors
            })
    }

    pub fn collect_interfaces(
        document: &schema::Document,
    ) -> Result<
        (
            BTreeMap<Name, Vec<InterfaceType>>,
            BTreeMap<Name, Vec<ObjectType>>,
        ),
        SchemaValidationError,
    > {
        // Initialize with an empty vec for each interface, so we don't
        // miss interfaces that have no implementors.
        let mut types_for_interface =
            BTreeMap::from_iter(document.definitions.iter().filter_map(|d| match d {
                schema::Definition::TypeDefinition(TypeDefinition::Interface(t)) => {
                    Some((t.name.clone(), vec![]))
                }
                _ => None,
            }));
        let mut interfaces_for_type = BTreeMap::<_, Vec<_>>::new();

        for object_type in traversal::get_object_type_definitions(&document) {
            for implemented_interface in object_type.implements_interfaces.clone() {
                let interface_type = document
                    .definitions
                    .iter()
                    .find_map(|def| match def {
                        schema::Definition::TypeDefinition(TypeDefinition::Interface(i))
                            if i.name == implemented_interface =>
                        {
                            Some(i.clone())
                        }
                        _ => None,
                    })
                    .ok_or_else(|| {
                        SchemaValidationError::UndefinedInterface(implemented_interface.clone())
                    })?;

                Self::validate_interface_implementation(object_type, &interface_type)?;

                interfaces_for_type
                    .entry(object_type.name.clone())
                    .or_default()
                    .push(interface_type);
                types_for_interface
                    .get_mut(&implemented_interface)
                    .unwrap()
                    .push(object_type.clone());
            }
        }

        return Ok((interfaces_for_type, types_for_interface));
    }

    pub fn parse(raw: &str, id: SubgraphDeploymentId) -> Result<Self, Error> {
        let document = graphql_parser::parse_schema(&raw)?;

        let (interfaces_for_type, types_for_interface) = Self::collect_interfaces(&document)?;

        let mut schema = Schema {
            id: id.clone(),
            document,
            interfaces_for_type,
            types_for_interface,
        };
        schema.add_subgraph_id_directives(id);

        Ok(schema)
    }

    pub fn imported_types(&self) -> HashMap<ImportedType, SchemaReference> {
        self.subgraph_schema_object_type()
            .map_or(HashMap::new(), |object| {
                object
                    .directives
                    .iter()
                    .filter(|directive| directive.name == "imports")
                    .map(|imports| {
                        imports
                            .arguments
                            .iter()
                            .find(|(name, _)| name == "from")
                            .map_or(vec![], |from| {
                                self.schema_reference_from_directive_argument(from).map_or(
                                    vec![],
                                    |schema_ref| {
                                        self.imported_types_from_import_directive(imports)
                                            .iter()
                                            .map(|imported_type| {
                                                (imported_type.clone(), schema_ref.clone())
                                            })
                                            .collect()
                                    },
                                )
                            })
                    })
                    .flatten()
                    .collect::<HashMap<ImportedType, SchemaReference>>()
            })
    }

    pub fn imported_schemas(&self) -> Vec<SchemaReference> {
        self.subgraph_schema_object_type().map_or(vec![], |object| {
            object
                .directives
                .iter()
                .filter(|directive| directive.name == "imports")
                .filter_map(|directive| directive.arguments.iter().find(|(name, _)| name == "from"))
                .filter_map(|from| self.schema_reference_from_directive_argument(from))
                .collect()
        })
    }

    fn imported_types_from_import_directive(&self, imports: &Directive) -> Vec<ImportedType> {
        imports
            .arguments
            .iter()
            .find(|(name, _)| name == "types")
            .map_or(vec![], |(_, value)| match value {
                Value::List(types) => types
                    .iter()
                    .filter_map(|import_type| match import_type {
                        Value::String(type_name) => Some(ImportedType::Name(type_name.to_string())),
                        Value::Object(type_name_as) => {
                            let name =
                                type_name_as
                                    .get("name")
                                    .and_then(|name_value| match name_value {
                                        Value::String(name) => Some(name.to_string()),
                                        _ => None,
                                    });
                            let az = type_name_as.get("as").and_then(|as_value| match as_value {
                                Value::String(az) => Some(az.to_string()),
                                _ => None,
                            });
                            match (name, az) {
                                (Some(name), Some(az)) => Some(ImportedType::NameAs(name, az)),
                                _ => None,
                            }
                        }
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            })
    }

    fn schema_reference_from_directive_argument(
        &self,
        from: &(Name, Value),
    ) -> Option<SchemaReference> {
        let (name, value) = from;
        if name != "from" {
            return None;
        }
        match value {
            Value::Object(map) => {
                let id = map
                    .get("id")
                    .filter(|id| match id {
                        Value::String(_) => true,
                        _ => false,
                    })
                    .map(|id| match id {
                        Value::String(i) => SchemaReference::ById(i.to_string()),
                        _ => unreachable!(),
                    });
                let name = map
                    .get("name")
                    .filter(|name| match name {
                        Value::String(_) => true,
                        _ => false,
                    })
                    .map(|name| match name {
                        Value::String(n) => SchemaReference::ByName(n.to_string()),
                        _ => unreachable!(),
                    });
                id.or(name)
            }
            _ => None,
        }
    }

    /// Returned map has one an entry for each interface in the schema.
    pub fn types_for_interface(&self) -> &BTreeMap<Name, Vec<ObjectType>> {
        &self.types_for_interface
    }

    /// Returns `None` if the type implements no interfaces.
    pub fn interfaces_for_type(&self, type_name: &Name) -> Option<&Vec<InterfaceType>> {
        self.interfaces_for_type.get(type_name)
    }

    // Adds a @subgraphId(id: ...) directive to object/interface/enum types in the schema.
    pub fn add_subgraph_id_directives(&mut self, id: SubgraphDeploymentId) {
        for definition in self.document.definitions.iter_mut() {
            let subgraph_id_argument = (
                schema::Name::from("id"),
                schema::Value::String(id.to_string()),
            );

            let subgraph_id_directive = schema::Directive {
                name: "subgraphId".to_string(),
                position: Pos::default(),
                arguments: vec![subgraph_id_argument],
            };

            if let schema::Definition::TypeDefinition(ref mut type_definition) = definition {
                let directives = match type_definition {
                    TypeDefinition::Object(object_type) => &mut object_type.directives,
                    TypeDefinition::Interface(interface_type) => &mut interface_type.directives,
                    TypeDefinition::Enum(enum_type) => &mut enum_type.directives,
                    TypeDefinition::Scalar(scalar_type) => &mut scalar_type.directives,
                    TypeDefinition::InputObject(input_object_type) => {
                        &mut input_object_type.directives
                    }
                    TypeDefinition::Union(union_type) => &mut union_type.directives,
                };

                if directives
                    .iter()
                    .find(|directive| directive.name == "subgraphId")
                    .is_none()
                {
                    directives.push(subgraph_id_directive);
                }
            };
        }
    }

    pub fn validate(
        &self,
        _schemas: &HashMap<SchemaReference, Arc<Schema>>,
    ) -> Result<(), Vec<SchemaValidationError>> {
        let mut errors = vec![];
        self.validate_schema_types()
            .unwrap_or_else(|err| errors.push(err));
        self.validate_derived_from()
            .unwrap_or_else(|err| errors.push(err));
        self.validate_subgraph_schema_has_no_fields()
            .unwrap_or_else(|err| errors.push(err));
        // Should validate that import directives are properly formed
        // Should verify that import directives only exist on the _Schema_ type
        self.validate_import_directives()
            .unwrap_or_else(|err| errors.push(err));
        // Should validate that all types in the Subgraph referenced from other subgraphs exist
        // If the referenced subgraph is not provided as an argument, do not validate those types
        self.validate_fields()
            .unwrap_or_else(|mut err| errors.append(&mut err));

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn validate_subgraph_schema_has_no_fields(&self) -> Result<(), SchemaValidationError> {
        match self
            .subgraph_schema_object_type()
            .and_then(|subgraph_schema_type| {
                if !subgraph_schema_type.fields.is_empty() {
                    Some(SchemaValidationError::SubgraphSchemaTypeFieldsInvalid)
                } else {
                    None
                }
            }) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn validate_import_directives(&self) -> Result<(), SchemaValidationError> {
        match self
            .subgraph_schema_object_type()
            .and_then(|subgraph_schema_type| {
                if !subgraph_schema_type
                    .directives
                    .iter()
                    .filter(|directive| directive.name != "imports")
                    .collect::<Vec<&Directive>>()
                    .is_empty()
                {
                    Some(SchemaValidationError::SubgraphSchemaDirectivesInvalid)
                } else {
                    subgraph_schema_type
                        .directives
                        .iter()
                        .filter(|directive| directive.name == "imports")
                        // TODO: Finish verifying import directive
                        // Each import directive must have a valid `from` argument
                        // Each import directive must have a valid 'types` argument
                        .find(|_directive| false)
                        .map(|_| SchemaValidationError::ImportDirectiveInvalid)
                }
            }) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn validate_imported_types(
        &self,
        _schemas: &HashMap<SchemaReference, Arc<Schema>>,
    ) -> Result<(), Vec<SchemaValidationError>> {
        Ok(())
    }

    fn validate_fields(&self) -> Result<(), Vec<SchemaValidationError>> {
        let native_types = traversal::get_object_and_interface_type_fields(&self.document);
        let imported_types = self.imported_types();

        // For each field in the root_schema, verify that the field
        // is either a: [BuiltInScalar, Native, Imported] type
        let errors = native_types
            .iter()
            .fold(vec![], |errors, (type_name, fields)| {
                fields.iter().fold(errors, |mut errors, field| {
                    let base = traversal::get_base_type(&field.field_type);
                    match BuiltInScalarType::try_from(base)
                        .map(|_| ())
                        .or_else(|_| match native_types.contains_key(base) {
                            true => Ok(()),
                            false => Err(()),
                        })
                        .or_else(|_| {
                            // Check imported types and the corresponding schema
                            imported_types
                                .iter()
                                .find(|(imported_type, _)| match imported_type {
                                    ImportedType::Name(name) if name == base => true,
                                    ImportedType::NameAs(_, az) if az == base => true,
                                    _ => false,
                                })
                                .map_or(Err(()), |_| Ok(()))
                        }) {
                        Err(_) => errors.push(SchemaValidationError::GraphQLTypeFieldInvalid(
                            type_name.to_string(),
                            field.name.to_string(),
                            base.to_string(),
                        )),
                        Ok(_) => (),
                    };
                    errors
                })
            });

        match errors.is_empty() {
            false => Err(errors),
            true => Ok(()),
        }
    }

    fn validate_schema_types(&self) -> Result<(), SchemaValidationError> {
        let types_without_entity_directive = traversal::get_object_type_definitions(&self.document)
            .iter()
            .filter(|t| traversal::get_object_type_directive(t, String::from("entity")).is_none())
            .map(|t| t.name.to_owned())
            .collect::<Vec<_>>();
        if types_without_entity_directive.is_empty() {
            Ok(())
        } else {
            Err(SchemaValidationError::EntityDirectivesMissing(Strings(
                types_without_entity_directive,
            )))
        }
    }

    fn validate_derived_from(&self) -> Result<(), SchemaValidationError> {
        // Helper to construct a DerivedFromInvalid
        fn invalid(
            object_type: &ObjectType,
            field_name: &str,
            reason: &str,
        ) -> SchemaValidationError {
            SchemaValidationError::DerivedFromInvalid(
                object_type.name.to_owned(),
                field_name.to_owned(),
                reason.to_owned(),
            )
        }

        let type_definitions = traversal::get_object_type_definitions(&self.document);
        let object_and_interface_type_fields =
            traversal::get_object_and_interface_type_fields(&self.document);

        // Iterate over all derived fields in all entity types; include the
        // interface types that the entity with the `@derivedFrom` implements
        // and the `field` argument of @derivedFrom directive
        for (object_type, interface_types, field, target_field) in type_definitions
            .clone()
            .iter()
            .flat_map(|object_type| {
                object_type
                    .fields
                    .iter()
                    .map(move |field| (object_type, field))
            })
            .filter_map(|(object_type, field)| {
                traversal::find_derived_from(field).map(|directive| {
                    (
                        object_type,
                        object_type
                            .implements_interfaces
                            .iter()
                            .filter(|iface| {
                                // Any interface that has `field` can be used
                                // as the type of the field
                                traversal::find_interface(&self.document, iface)
                                    .map(|iface| {
                                        iface.fields.iter().any(|ifield| ifield.name == field.name)
                                    })
                                    .unwrap_or(false)
                            })
                            .collect::<Vec<_>>(),
                        field,
                        directive
                            .arguments
                            .iter()
                            .find(|(name, _)| name == "field")
                            .map(|(_, value)| value),
                    )
                })
            })
        {
            // Turn `target_field` into the string name of the field
            let target_field = target_field.ok_or_else(|| {
                invalid(
                    object_type,
                    &field.name,
                    "the @derivedFrom directive must have a `field` argument",
                )
            })?;
            let target_field = match target_field {
                Value::String(s) => s,
                _ => {
                    return Err(invalid(
                        object_type,
                        &field.name,
                        "the value of the @derivedFrom `field` argument must be a string",
                    ))
                }
            };

            // Check that the type we are deriving from exists
            let target_type_name = traversal::get_base_type(&field.field_type);
            let target_fields = object_and_interface_type_fields
                .get(target_type_name)
                .ok_or_else(|| {
                    invalid(
                        object_type,
                        &field.name,
                        "the type of the field must be an existing entity or interface type",
                    )
                })?;

            // Check that the type we are deriving from has a field with the
            // right name and type
            let target_field = target_fields
                .iter()
                .find(|field| &field.name == target_field)
                .ok_or_else(|| {
                    let msg = format!(
                        "field `{}` does not exist on type `{}`",
                        target_field, target_type_name
                    );
                    invalid(object_type, &field.name, &msg)
                })?;

            // The field we are deriving from has to point back to us; as an
            // exception, we allow deriving from the `id` of another type.
            // For that, we will wind up comparing the `id`s of the two types
            // when we query, and just assume that that's ok.
            let target_field_type = traversal::get_base_type(&target_field.field_type);
            if target_field_type != &object_type.name
                && target_field_type != "ID"
                && !interface_types
                    .iter()
                    .any(|iface| &target_field_type == iface)
            {
                fn type_signatures(name: &String) -> Vec<String> {
                    vec![
                        format!("{}", name),
                        format!("{}!", name),
                        format!("[{}!]", name),
                        format!("[{}!]!", name),
                    ]
                }

                let mut valid_types = type_signatures(&object_type.name);
                valid_types.extend(
                    interface_types
                        .iter()
                        .flat_map(|iface| type_signatures(iface)),
                );
                let valid_types = valid_types.join(", ");

                let msg = format!(
                    "field `{tf}` on type `{tt}` must have one of the following type: {valid_types}",
                    tf = target_field.name,
                    tt = target_type_name,
                    valid_types = valid_types,
                );
                return Err(invalid(object_type, &field.name, &msg));
            }
        }
        Ok(())
    }

    /// Validate `interfaceethat `object` implements `interface`.
    fn validate_interface_implementation(
        object: &ObjectType,
        interface: &InterfaceType,
    ) -> Result<(), SchemaValidationError> {
        // Check that all fields in the interface exist in the object with same name and type.
        let mut missing_fields = vec![];
        for i in &interface.fields {
            if object
                .fields
                .iter()
                .find(|o| o.name == i.name && o.field_type == i.field_type)
                .is_none()
            {
                missing_fields.push(i.to_string().trim().to_owned());
            }
        }
        if !missing_fields.is_empty() {
            Err(SchemaValidationError::CannotImplement(
                object.name.clone(),
                interface.name.clone(),
                Strings(missing_fields),
            ))
        } else {
            Ok(())
        }
    }

    fn subgraph_schema_object_type(&self) -> Option<&ObjectType> {
        traversal::get_object_type_definitions(&self.document)
            .into_iter()
            .find(|object_type| object_type.name == SCHEMA_TYPE_NAME)
    }
}

#[test]
fn non_existing_interface() {
    let schema = "type Foo implements Bar @entity { foo: Int }";
    let res = Schema::parse(schema, SubgraphDeploymentId::new("dummy").unwrap());
    let error = res
        .unwrap_err()
        .downcast::<SchemaValidationError>()
        .unwrap();
    assert_eq!(
        error,
        SchemaValidationError::UndefinedInterface("Bar".to_owned())
    );
}

#[test]
fn invalid_interface_implementation() {
    let schema = "
        interface Foo {
            x: Int,
            y: Int
        }

        type Bar implements Foo @entity {
            x: Boolean
        }
    ";
    let res = Schema::parse(schema, SubgraphDeploymentId::new("dummy").unwrap());
    assert_eq!(
        res.unwrap_err().to_string(),
        "Entity type `Bar` cannot implement `Foo` because it is missing the \
         required fields: x: Int, y: Int"
    );
}
