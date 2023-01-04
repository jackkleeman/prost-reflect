use prost::bytes::Bytes;

use crate::{
    descriptor::{
        build::{
            join_path, resolve_name,
            visit::{visit, Visitor},
            DescriptorPoolOffsets,
        },
        error::{DescriptorError, DescriptorErrorKind, Label},
        find_enum_proto, find_message_proto, tag, to_index,
        types::{
            field_descriptor_proto, DescriptorProto, EnumValueDescriptorProto,
            FieldDescriptorProto, FileDescriptorProto, MethodDescriptorProto,
            ServiceDescriptorProto,
        },
        Definition, DefinitionKind, DescriptorPoolInner, EnumIndex, EnumValueIndex,
        ExtensionDescriptorInner, ExtensionIndex, FieldDescriptorInner, FieldIndex, FileIndex,
        Identity, KindIndex, MessageIndex, MethodDescriptorInner, MethodIndex, OneofIndex,
        ServiceDescriptorInner, ServiceIndex, RESERVED_MESSAGE_FIELD_NUMBERS,
        VALID_MESSAGE_FIELD_NUMBERS,
    },
    Cardinality, Syntax, Value,
};

impl DescriptorPoolInner {
    pub(super) fn resolve_names<'a>(
        &mut self,
        offsets: DescriptorPoolOffsets,
        files: impl Iterator<Item = &'a FileDescriptorProto>,
    ) -> Result<(), DescriptorError> {
        let mut visitor = ResolveVisitor {
            pool: self,
            errors: vec![],
        };
        visit(offsets, files, &mut visitor);
        if visitor.errors.is_empty() {
            Ok(())
        } else {
            Err(DescriptorError::new(visitor.errors))
        }
    }
}

struct ResolveVisitor<'a> {
    pool: &'a mut DescriptorPoolInner,
    errors: Vec<DescriptorErrorKind>,
}

impl<'a> Visitor for ResolveVisitor<'a> {
    fn visit_file(&mut self, path: &[i32], index: FileIndex, file: &FileDescriptorProto) {
        for (i, dependency) in file.dependency.iter().enumerate() {
            if let Some(&dependency_index) = self.pool.file_names.get(dependency.as_str()) {
                self.pool.files[index as usize]
                    .dependencies
                    .push(dependency_index);
            } else {
                self.errors.push(DescriptorErrorKind::FileNotFound {
                    name: dependency.clone(),
                    found: Label::new(
                        &self.pool.files,
                        "found here",
                        index,
                        join_path(path, &[tag::file::DEPENDENCY, i as i32]),
                    ),
                });
            }
        }
        for &public_dependency in &file.public_dependency {
            if !matches!(usize::try_from(public_dependency), Ok(i) if i < file.dependency.len()) {
                self.errors.push(DescriptorErrorKind::InvalidImportIndex);
            }
        }
        for &weak_dependency in &file.weak_dependency {
            if !matches!(usize::try_from(weak_dependency), Ok(i) if i < file.dependency.len()) {
                self.errors.push(DescriptorErrorKind::InvalidImportIndex);
            }
        }
    }

    fn visit_field(
        &mut self,
        path: &[i32],
        full_name: &str,
        file: FileIndex,
        message: MessageIndex,
        index: FieldIndex,
        field: &FieldDescriptorProto,
    ) {
        debug_assert_eq!(
            to_index(self.pool.messages[message as usize].fields.len()),
            index
        );

        let syntax = self.pool.files[file as usize].syntax;

        self.check_field_number(message, field, file, path);

        let cardinality = match field.label() {
            field_descriptor_proto::Label::Optional => Cardinality::Optional,
            field_descriptor_proto::Label::Required => Cardinality::Required,
            field_descriptor_proto::Label::Repeated => Cardinality::Repeated,
        };

        let kind = self.resolve_field_type(field.r#type, field.type_name(), full_name, file, path);

        let is_packed = cardinality == Cardinality::Repeated
            && kind.map_or(false, |k| k.is_packable())
            && (field
                .options
                .as_ref()
                .map_or(syntax == Syntax::Proto3, |o| o.value.packed()));

        let supports_presence = field.proto3_optional()
            || field.oneof_index.is_some()
            || (cardinality != Cardinality::Repeated
                && (kind.map_or(false, |k| k.is_message()) || syntax == Syntax::Proto2));

        let default = kind.ok().and_then(|kind| {
            self.parse_field_default_value(kind, field.default_value.as_deref(), file, path)
        });

        let message = &mut self.pool.messages[message as usize];

        let oneof = field.oneof_index.and_then(|oneof_index| {
            if oneof_index < 0 || oneof_index as usize >= message.oneofs.len() {
                self.errors.push(DescriptorErrorKind::InvalidOneofIndex);
                None
            } else {
                message.oneofs[oneof_index as usize].fields.push(index);
                Some(oneof_index as OneofIndex)
            }
        });

        message.fields.push(FieldDescriptorInner {
            id: Identity::new(file, path, full_name, field.name()),
            number: field.number() as u32,
            kind: kind.unwrap_or(KindIndex::Double),
            oneof,
            is_packed,
            supports_presence,
            cardinality,
            default,
        });
        if let Some(existing) = message.field_numbers.insert(field.number() as u32, index) {
            self.errors.push(DescriptorErrorKind::DuplicateFieldNumber {
                number: field.number() as u32,
                first: Label::new(
                    &self.pool.files,
                    "first defined here",
                    file,
                    join_path(
                        &message.fields[existing as usize].id.path,
                        &[tag::field::NUMBER],
                    ),
                ),
                second: Label::new(
                    &self.pool.files,
                    "defined again here",
                    file,
                    join_path(path, &[tag::field::NUMBER]),
                ),
            });
        }
        if let Some(existing) = message.field_names.insert(field.name().into(), index) {
            self.errors.push(DescriptorErrorKind::DuplicateName {
                name: full_name.to_owned(),
                first: Label::new(
                    &self.pool.files,
                    "first defined here",
                    file,
                    join_path(
                        &message.fields[existing as usize].id.path,
                        &[tag::field::NAME],
                    ),
                ),
                second: Label::new(
                    &self.pool.files,
                    "defined again here",
                    file,
                    join_path(path, &[tag::field::NAME]),
                ),
            });
        }
        if let Some(existing) = message
            .field_json_names
            .insert(field.json_name().into(), index)
        {
            self.errors
                .push(DescriptorErrorKind::DuplicateFieldJsonName {
                    name: field.json_name().to_owned(),
                    first: Label::new(
                        &self.pool.files,
                        "first defined here",
                        file,
                        join_path(
                            &message.fields[existing as usize].id.path,
                            &[tag::field::NAME],
                        ),
                    ),
                    second: Label::new(
                        &self.pool.files,
                        "defined again here",
                        file,
                        join_path(path, &[tag::field::NAME]),
                    ),
                });
        }
    }

    fn visit_service(
        &mut self,
        path: &[i32],
        full_name: &str,
        file: FileIndex,
        index: ServiceIndex,
        service: &ServiceDescriptorProto,
    ) {
        debug_assert_eq!(to_index(self.pool.services.len()), index);

        self.pool.services.push(ServiceDescriptorInner {
            id: Identity::new(file, path, full_name, service.name()),
            methods: Vec::with_capacity(service.method.len()),
        });
    }

    fn visit_method(
        &mut self,
        path: &[i32],
        full_name: &str,
        file: FileIndex,
        service: ServiceIndex,
        index: MethodIndex,
        method: &MethodDescriptorProto,
    ) {
        debug_assert_eq!(
            to_index(self.pool.services[service as usize].methods.len()),
            index
        );

        let input = self
            .find_message(
                full_name,
                method.input_type(),
                file,
                path,
                tag::method::INPUT_TYPE,
            )
            .unwrap_or(MessageIndex::MAX);
        let output = self
            .find_message(
                full_name,
                method.output_type(),
                file,
                path,
                tag::method::OUTPUT_TYPE,
            )
            .unwrap_or(MessageIndex::MAX);

        self.pool.services[service as usize]
            .methods
            .push(MethodDescriptorInner {
                id: Identity::new(file, path, full_name, method.name()),
                input,
                output,
            });
    }

    fn visit_enum_value(
        &mut self,
        path: &[i32],
        full_name: &str,
        file: FileIndex,
        enum_index: EnumIndex,
        index: EnumValueIndex,
        value: &EnumValueDescriptorProto,
    ) {
        self.check_enum_number(enum_index, value, file, path);

        let enum_ = &mut self.pool.enums[enum_index as usize];

        let value_numbers_index = match enum_
            .value_numbers
            .binary_search_by(|(number, _)| number.cmp(&value.number()))
        {
            Ok(existing_index) => {
                if !enum_.allow_alias {
                    let existing = enum_.value_numbers[existing_index].1;
                    self.errors.push(DescriptorErrorKind::DuplicateEnumNumber {
                        number: value.number(),
                        first: Label::new(
                            &self.pool.files,
                            "first defined here",
                            file,
                            join_path(
                                &enum_.values[existing as usize].id.path,
                                &[tag::enum_value::NUMBER],
                            ),
                        ),
                        second: Label::new(
                            &self.pool.files,
                            "defined again here",
                            file,
                            join_path(path, &[tag::enum_value::NUMBER]),
                        ),
                    });
                }
                existing_index
            }
            Err(index) => index,
        };
        enum_
            .value_numbers
            .insert(value_numbers_index, (value.number(), index));

        if let Some(existing) = enum_.value_names.insert(value.name().into(), index) {
            self.errors.push(DescriptorErrorKind::DuplicateName {
                name: full_name.to_owned(),
                first: Label::new(
                    &self.pool.files,
                    "first defined here",
                    file,
                    join_path(
                        &enum_.values[existing as usize].id.path,
                        &[tag::enum_value::NAME],
                    ),
                ),
                second: Label::new(
                    &self.pool.files,
                    "defined again here",
                    file,
                    join_path(path, &[tag::enum_value::NAME]),
                ),
            });
        }
    }

    fn visit_extension(
        &mut self,
        path: &[i32],
        full_name: &str,
        file: FileIndex,
        parent_message: Option<MessageIndex>,
        index: ExtensionIndex,
        extension: &FieldDescriptorProto,
    ) {
        debug_assert_eq!(to_index(self.pool.extensions.len()), index);

        let extendee = self.find_message(
            full_name,
            extension.extendee(),
            file,
            path,
            tag::field::EXTENDEE,
        );
        if let Some(extendee) = extendee {
            self.pool.messages[extendee as usize].extensions.push(index);

            self.check_field_number(extendee, extension, file, path);
        }

        let syntax = self.pool.files[file as usize].syntax;

        let cardinality = match extension.label() {
            field_descriptor_proto::Label::Optional => Cardinality::Optional,
            field_descriptor_proto::Label::Required => Cardinality::Required,
            field_descriptor_proto::Label::Repeated => Cardinality::Repeated,
        };

        let kind = self.resolve_field_type(
            extension.r#type,
            extension.type_name(),
            full_name,
            file,
            path,
        );

        let is_packed = cardinality == Cardinality::Repeated
            && kind.map_or(false, |k| k.is_packable())
            && (extension
                .options
                .as_ref()
                .map_or(syntax == Syntax::Proto3, |o| o.value.packed()));

        let default = kind.ok().and_then(|kind| {
            self.parse_field_default_value(kind, extension.default_value.as_deref(), file, path)
        });

        self.pool.extensions.push(ExtensionDescriptorInner {
            id: Identity::new(file, path, full_name, extension.name()),
            parent: parent_message,
            number: extension.number() as u32,
            json_name: format!("[{}]", full_name).into(),
            extendee: extendee.unwrap_or(MessageIndex::MAX),
            kind: kind.unwrap_or(KindIndex::Double),
            is_packed,
            cardinality,
            default,
        });
    }
}

impl<'a> ResolveVisitor<'a> {
    fn check_field_number(
        &mut self,
        message: MessageIndex,
        field: &FieldDescriptorProto,
        file: FileIndex,
        path: &[i32],
    ) {
        if !VALID_MESSAGE_FIELD_NUMBERS.contains(&field.number())
            || RESERVED_MESSAGE_FIELD_NUMBERS.contains(&field.number())
        {
            self.errors.push(DescriptorErrorKind::InvalidFieldNumber {
                number: field.number(),
                found: Label::new(
                    &self.pool.files,
                    "defined here",
                    file,
                    join_path(path, &[tag::field::NUMBER]),
                ),
            });
        }

        let message = &self.pool.messages[message as usize];
        let message_proto = find_message_proto(
            &self.pool.files[message.id.file as usize].raw,
            &message.id.path,
        );
        for (i, range) in message_proto.reserved_range.iter().enumerate() {
            if range.start() <= field.number() && field.number() < range.end() {
                self.errors
                    .push(DescriptorErrorKind::FieldNumberInReservedRange {
                        number: field.number(),
                        range: range.start()..range.end(),
                        defined: Label::new(
                            &self.pool.files,
                            "reserved range defined here",
                            message.id.file,
                            join_path(&message.id.path, &[tag::message::RESERVED_RANGE, i as i32]),
                        ),
                        found: Label::new(
                            &self.pool.files,
                            "defined here",
                            file,
                            join_path(path, &[tag::field::NUMBER]),
                        ),
                    });
            }
        }

        let extension_range = message_proto
            .extension_range
            .iter()
            .enumerate()
            .find(|(_, range)| range.start() <= field.number() && field.number() < range.end());
        match (&field.extendee, extension_range) {
            (None, None) | (Some(_), Some(_)) => (),
            (None, Some((i, range))) => {
                self.errors
                    .push(DescriptorErrorKind::FieldNumberInExtensionRange {
                        number: field.number(),
                        range: range.start()..range.end(),
                        defined: Label::new(
                            &self.pool.files,
                            "extension range defined here",
                            message.id.file,
                            join_path(&message.id.path, &[tag::message::EXTENSION_RANGE, i as i32]),
                        ),
                        found: Label::new(
                            &self.pool.files,
                            "defined here",
                            file,
                            join_path(path, &[tag::field::NUMBER]),
                        ),
                    });
            }
            (Some(_), None) => {
                self.errors
                    .push(DescriptorErrorKind::ExtensionNumberOutOfRange {
                        number: field.number(),
                        message: message.id.full_name().to_owned(),
                        found: Label::new(
                            &self.pool.files,
                            "defined here",
                            file,
                            join_path(path, &[tag::field::NUMBER]),
                        ),
                    });
            }
        }
    }

    fn check_enum_number(
        &mut self,
        enum_: EnumIndex,
        value: &EnumValueDescriptorProto,
        file: FileIndex,
        path: &[i32],
    ) {
        let enum_ = &self.pool.enums[enum_ as usize];
        let enum_proto =
            find_enum_proto(&self.pool.files[enum_.id.file as usize].raw, &enum_.id.path);
        for (i, range) in enum_proto.reserved_range.iter().enumerate() {
            if range.start() <= value.number() && value.number() <= range.end() {
                self.errors
                    .push(DescriptorErrorKind::EnumNumberInReservedRange {
                        number: value.number(),
                        range: range.start()..=range.end(),
                        defined: Label::new(
                            &self.pool.files,
                            "reserved range defined here",
                            enum_.id.file,
                            join_path(&enum_.id.path, &[tag::enum_::RESERVED_RANGE, i as i32]),
                        ),
                        found: Label::new(
                            &self.pool.files,
                            "defined here",
                            file,
                            join_path(path, &[tag::field::NUMBER]),
                        ),
                    });
            }
        }
    }

    fn resolve_field_type(
        &mut self,
        ty: Option<i32>,
        ty_name: &str,
        scope: &str,
        file: FileIndex,
        path: &[i32],
    ) -> Result<KindIndex, ()> {
        if ty_name.is_empty() {
            let ty = match ty.and_then(field_descriptor_proto::Type::from_i32) {
                Some(ty) => ty,
                None => {
                    self.add_missing_required_field_error(
                        file,
                        join_path(path, &[tag::field::TYPE]),
                    );
                    return Err(());
                }
            };

            match ty {
                field_descriptor_proto::Type::Double => Ok(KindIndex::Double),
                field_descriptor_proto::Type::Float => Ok(KindIndex::Float),
                field_descriptor_proto::Type::Int64 => Ok(KindIndex::Int64),
                field_descriptor_proto::Type::Uint64 => Ok(KindIndex::Uint64),
                field_descriptor_proto::Type::Int32 => Ok(KindIndex::Int32),
                field_descriptor_proto::Type::Fixed64 => Ok(KindIndex::Fixed64),
                field_descriptor_proto::Type::Fixed32 => Ok(KindIndex::Fixed32),
                field_descriptor_proto::Type::Bool => Ok(KindIndex::Bool),
                field_descriptor_proto::Type::String => Ok(KindIndex::String),
                field_descriptor_proto::Type::Bytes => Ok(KindIndex::Bytes),
                field_descriptor_proto::Type::Uint32 => Ok(KindIndex::Uint32),
                field_descriptor_proto::Type::Sfixed32 => Ok(KindIndex::Sfixed32),
                field_descriptor_proto::Type::Sfixed64 => Ok(KindIndex::Sfixed64),
                field_descriptor_proto::Type::Sint32 => Ok(KindIndex::Sint32),
                field_descriptor_proto::Type::Sint64 => Ok(KindIndex::Sint64),
                field_descriptor_proto::Type::Group
                | field_descriptor_proto::Type::Message
                | field_descriptor_proto::Type::Enum => {
                    self.add_missing_required_field_error(
                        file,
                        join_path(path, &[tag::field::TYPE_NAME]),
                    );
                    Err(())
                }
            }
        } else {
            match self.resolve_name(scope, ty_name, file, path, tag::field::TYPE_NAME) {
                Some(Definition {
                    kind: DefinitionKind::Message(message),
                    ..
                }) => {
                    if ty == Some(field_descriptor_proto::Type::Group as i32) {
                        Ok(KindIndex::Group(*message))
                    } else {
                        Ok(KindIndex::Message(*message))
                    }
                }
                Some(Definition {
                    kind: DefinitionKind::Enum(enum_),
                    ..
                }) => Ok(KindIndex::Enum(*enum_)),
                Some(def) => {
                    let def_file = def.file;
                    let def_path = def.path.clone();

                    self.errors.push(DescriptorErrorKind::InvalidType {
                        name: ty_name.to_owned(),
                        expected: "a message or enum type".to_owned(),
                        found: Label::new(
                            &self.pool.files,
                            "found here",
                            file,
                            join_path(path, &[tag::field::TYPE_NAME]),
                        ),
                        defined: Label::new(&self.pool.files, "defined here", def_file, def_path),
                    });
                    Err(())
                }
                None => {
                    self.errors.push(DescriptorErrorKind::NameNotFound {
                        name: ty_name.to_owned(),
                        found: Label::new(
                            &self.pool.files,
                            "found here",
                            file,
                            join_path(path, &[tag::field::TYPE_NAME]),
                        ),
                    });
                    Err(())
                }
            }
        }
    }

    fn parse_field_default_value(
        &mut self,
        kind: KindIndex,
        default_value: Option<&str>,
        file: FileIndex,
        path: &[i32],
    ) -> Option<Value> {
        let default_value = match default_value {
            Some(value) => value,
            None => return None,
        };

        match kind {
            KindIndex::Double
            | KindIndex::Float
            | KindIndex::Int32
            | KindIndex::Int64
            | KindIndex::Uint32
            | KindIndex::Uint64
            | KindIndex::Sint32
            | KindIndex::Sint64
            | KindIndex::Fixed32
            | KindIndex::Fixed64
            | KindIndex::Sfixed32
            | KindIndex::Sfixed64
            | KindIndex::Bool
            | KindIndex::String
            | KindIndex::Bytes => match parse_simple_value(kind, default_value) {
                Ok(value) => Some(value),
                Err(_) => {
                    self.errors.push(DescriptorErrorKind::InvalidFieldDefault {
                        value: default_value.to_owned(),
                        kind: format!("{:?}", kind),
                        found: Label::new(
                            &self.pool.files,
                            "found here",
                            file,
                            join_path(path, &[tag::field::DEFAULT_VALUE]),
                        ),
                    });
                    None
                }
            },
            KindIndex::Enum(enum_) => {
                let enum_ = &self.pool.enums[enum_ as usize];
                if let Some(value) = enum_.values.iter().find(|v| v.id.name() == default_value) {
                    Some(Value::EnumNumber(value.number))
                } else {
                    self.errors.push(DescriptorErrorKind::InvalidFieldDefault {
                        value: default_value.to_owned(),
                        kind: enum_.id.full_name().to_owned(),
                        found: Label::new(
                            &self.pool.files,
                            "found here",
                            file,
                            join_path(path, &[tag::field::DEFAULT_VALUE]),
                        ),
                    });
                    None
                }
            }
            _ => {
                self.errors.push(DescriptorErrorKind::InvalidFieldDefault {
                    value: default_value.to_owned(),
                    kind: "message type".to_owned(),
                    found: Label::new(
                        &self.pool.files,
                        "found here",
                        file,
                        join_path(path, &[tag::field::DEFAULT_VALUE]),
                    ),
                });
                None
            }
        }
    }

    fn find_message(
        &mut self,
        scope: &str,
        name: &str,
        file: FileIndex,
        path1: &[i32],
        path2: i32,
    ) -> Option<MessageIndex> {
        match self.resolve_name(scope, name, file, path1, path2) {
            Some(Definition {
                kind: DefinitionKind::Message(message),
                ..
            }) => Some(*message),
            Some(def) => {
                let def_file = def.file;
                let def_path = def.path.clone();

                self.errors.push(DescriptorErrorKind::InvalidType {
                    name: name.to_owned(),
                    expected: "a message type".to_owned(),
                    found: Label::new(
                        &self.pool.files,
                        "found here",
                        file,
                        join_path(path1, &[path2]),
                    ),
                    defined: Label::new(&self.pool.files, "defined here", def_file, def_path),
                });
                None
            }
            None => {
                self.errors.push(DescriptorErrorKind::NameNotFound {
                    name: name.to_owned(),
                    found: Label::new(
                        &self.pool.files,
                        "found here",
                        file,
                        join_path(path1, &[path2]),
                    ),
                });
                None
            }
        }
    }

    fn resolve_name(
        &mut self,
        scope: &str,
        name: &str,
        file: FileIndex,
        path: &[i32],
        tag: i32,
    ) -> Option<&Definition> {
        if let Some((type_name, def)) = resolve_name(&self.pool.names, scope, name) {
            let ty = if matches!(
                def,
                Definition {
                    kind: DefinitionKind::Message(_),
                    ..
                }
            ) {
                field_descriptor_proto::Type::Message
            } else {
                field_descriptor_proto::Type::Enum
            };
            set_file_type_name(
                &mut self.pool.files[file as usize].raw,
                path,
                tag,
                type_name.into_owned(),
                ty,
            );
            Some(def)
        } else {
            None
        }
    }

    fn add_missing_required_field_error(&mut self, file: FileIndex, path: Box<[i32]>) {
        self.errors.push(DescriptorErrorKind::MissingRequiredField {
            label: Label::new(&self.pool.files, "found here", file, path),
        });
    }
}

fn parse_simple_value(
    kind: KindIndex,
    value: &str,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let value = match kind {
        KindIndex::Double => value.parse().map(Value::F64)?,
        KindIndex::Float => value.parse().map(Value::F32)?,
        KindIndex::Int32 | KindIndex::Sint32 | KindIndex::Sfixed32 => {
            value.parse().map(Value::I32)?
        }
        KindIndex::Int64 | KindIndex::Sint64 | KindIndex::Sfixed64 => {
            value.parse().map(Value::I64)?
        }
        KindIndex::Uint32 | KindIndex::Fixed32 => value.parse().map(Value::U32)?,
        KindIndex::Uint64 | KindIndex::Fixed64 => value.parse().map(Value::U64)?,
        KindIndex::Bool => value.parse().map(Value::Bool)?,
        KindIndex::String => Value::String(value.to_owned()),
        KindIndex::Bytes => unescape_c_escape_string(value).map(Value::Bytes)?,
        KindIndex::Enum(_) | KindIndex::Message(_) | KindIndex::Group(_) => unreachable!(),
    };

    Ok(value)
}

/// From https://github.com/tokio-rs/prost/blob/c3b7037a7f2c56cef327b41ca32a8c4e9ce5a41c/prost-build/src/code_generator.rs#L887
/// Based on [`google::protobuf::UnescapeCEscapeString`][1]
/// [1]: https://github.com/google/protobuf/blob/3.3.x/src/google/protobuf/stubs/strutil.cc#L312-L322
fn unescape_c_escape_string(s: &str) -> Result<Bytes, &'static str> {
    let src = s.as_bytes();
    let len = src.len();
    let mut dst = Vec::new();

    let mut p = 0;

    while p < len {
        if src[p] != b'\\' {
            dst.push(src[p]);
            p += 1;
        } else {
            p += 1;
            if p == len {
                return Err("missing escape character");
            }
            match src[p] {
                b'a' => {
                    dst.push(0x07);
                    p += 1;
                }
                b'b' => {
                    dst.push(0x08);
                    p += 1;
                }
                b'f' => {
                    dst.push(0x0C);
                    p += 1;
                }
                b'n' => {
                    dst.push(0x0A);
                    p += 1;
                }
                b'r' => {
                    dst.push(0x0D);
                    p += 1;
                }
                b't' => {
                    dst.push(0x09);
                    p += 1;
                }
                b'v' => {
                    dst.push(0x0B);
                    p += 1;
                }
                b'\\' => {
                    dst.push(0x5C);
                    p += 1;
                }
                b'?' => {
                    dst.push(0x3F);
                    p += 1;
                }
                b'\'' => {
                    dst.push(0x27);
                    p += 1;
                }
                b'"' => {
                    dst.push(0x22);
                    p += 1;
                }
                b'0'..=b'7' => {
                    let mut octal = 0;
                    for _ in 0..3 {
                        if p < len && src[p] >= b'0' && src[p] <= b'7' {
                            octal = octal * 8 + (src[p] - b'0');
                            p += 1;
                        } else {
                            break;
                        }
                    }
                    dst.push(octal);
                }
                b'x' | b'X' => {
                    if p + 3 > len {
                        return Err("hex escape must contain two characters");
                    }
                    match u8::from_str_radix(&s[p + 1..p + 3], 16) {
                        Ok(b) => dst.push(b),
                        _ => return Err("invalid hex escape"),
                    }
                    p += 3;
                }
                _ => return Err("invalid escape character"),
            }
        }
    }
    Ok(dst.into())
}

fn set_file_type_name(
    file: &mut FileDescriptorProto,
    path: &[i32],
    tag: i32,
    type_name: String,
    ty: field_descriptor_proto::Type,
) {
    match path[0] {
        tag::file::MESSAGE_TYPE => {
            let message = &mut file.message_type[path[1] as usize];
            set_message_type_name(message, &path[2..], tag, type_name, ty);
        }
        tag::file::SERVICE => {
            debug_assert_eq!(path.len(), 4);
            let service = &mut file.service[path[1] as usize];
            debug_assert_eq!(path[2], tag::service::METHOD);
            let method = &mut service.method[path[3] as usize];
            match tag {
                tag::method::INPUT_TYPE => method.input_type = Some(type_name),
                tag::method::OUTPUT_TYPE => method.output_type = Some(type_name),
                p => panic!("unknown path element {}", p),
            }
        }
        tag::file::EXTENSION => {
            debug_assert_eq!(path.len(), 2);
            let extension = &mut file.extension[path[1] as usize];
            set_field_type_name(extension, tag, type_name, ty);
        }
        p => panic!("unknown path element {}", p),
    }
}

fn set_message_type_name(
    message: &mut DescriptorProto,
    path: &[i32],
    tag: i32,
    type_name: String,
    ty: field_descriptor_proto::Type,
) {
    match path[0] {
        tag::message::FIELD => {
            debug_assert_eq!(path.len(), 2);
            let field = &mut message.field[path[1] as usize];
            set_field_type_name(field, tag, type_name, ty);
        }
        tag::message::EXTENSION => {
            debug_assert_eq!(path.len(), 2);
            let extension = &mut message.extension[path[1] as usize];
            set_field_type_name(extension, tag, type_name, ty);
        }
        tag::message::NESTED_TYPE => {
            let nested_message = &mut message.nested_type[path[1] as usize];
            set_message_type_name(nested_message, &path[2..], tag, type_name, ty);
        }
        p => panic!("unknown path element {}", p),
    }
}

fn set_field_type_name(
    field: &mut FieldDescriptorProto,
    tag: i32,
    type_name: String,
    ty: field_descriptor_proto::Type,
) {
    match tag {
        tag::field::TYPE_NAME => {
            field.type_name = Some(type_name);
            if field.r#type() != field_descriptor_proto::Type::Group {
                field.set_type(ty);
            }
        }
        tag::field::EXTENDEE => field.extendee = Some(type_name),
        p => panic!("unknown path element {}", p),
    }
}

#[test]
fn test_unescape_c_escape_string() {
    assert_eq!(Ok(Bytes::default()), unescape_c_escape_string(""));
    assert_eq!(
        Ok(Bytes::from_static(b"hello world")),
        unescape_c_escape_string("hello world"),
    );
    assert_eq!(
        Ok(Bytes::from_static(b"\0")),
        unescape_c_escape_string(r#"\0"#),
    );
    assert_eq!(
        Ok(Bytes::from_static(&[0o012, 0o156])),
        unescape_c_escape_string(r#"\012\156"#),
    );
    assert_eq!(
        Ok(Bytes::from_static(&[0x01, 0x02])),
        unescape_c_escape_string(r#"\x01\x02"#)
    );
    assert_eq!(
        Ok(Bytes::from_static(
            b"\0\x01\x07\x08\x0C\n\r\t\x0B\\\'\"\xFE?"
        )),
        unescape_c_escape_string(r#"\0\001\a\b\f\n\r\t\v\\\'\"\xfe\?"#),
    );
    assert_eq!(
        Err("hex escape must contain two characters"),
        unescape_c_escape_string(r#"\x"#)
    );
    assert_eq!(
        Err("hex escape must contain two characters"),
        unescape_c_escape_string(r#"\x1"#)
    );
    assert_eq!(
        Ok(Bytes::from_static(b"\x11")),
        unescape_c_escape_string(r#"\x11"#),
    );
    assert_eq!(
        Ok(Bytes::from_static(b"\x111")),
        unescape_c_escape_string(r#"\x111"#),
    );
    assert_eq!(
        Err("invalid escape character"),
        unescape_c_escape_string(r#"\w"#)
    );
    assert_eq!(
        Err("invalid hex escape"),
        unescape_c_escape_string(r#"\x__"#)
    );
}
