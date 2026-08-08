use std::io::{Read, Write};

use rbx_dom_weak::types::{Content, ContentId, ContentType};

use crate::{
    core::XmlType,
    deserializer::ParseState,
    deserializer_core::{XmlEventReader, XmlReadEvent},
    error::{DecodeError, DecodeErrorKind, EncodeError},
    serializer::EmitState,
    serializer_core::{XmlEventWriter, XmlWriteEvent},
};

pub const XML_TAG_NAME: &str = "Content";

pub fn write_content<W: Write>(
    writer: &mut XmlEventWriter<W>,
    xml_property_name: &str,
    value: &Content,
    state: &mut EmitState,
) -> Result<(), EncodeError> {
    writer.write(XmlWriteEvent::start_element(XML_TAG_NAME).attr("name", xml_property_name))?;
    match value.value() {
        ContentType::Object(referent) => {
            writer.write(XmlWriteEvent::start_element("Ref"))?;
            if referent.is_none() {
                writer.write(XmlWriteEvent::characters("null"))?;
            } else {
                writer.write_characters(state.map_id(*referent))?;
            }
            writer.write(XmlWriteEvent::end_element())?;
        }
        _ => value.write_xml(writer)?,
    }
    writer.write(XmlWriteEvent::end_element())?;
    Ok(())
}

pub fn read_content<R: Read>(
    reader: &mut XmlEventReader<R>,
    instance_id: rbx_dom_weak::types::Ref,
    property_name: &str,
    state: &mut ParseState,
) -> Result<Content, DecodeError> {
    reader.expect_start_with_name(XML_TAG_NAME)?;
    let value = match reader.expect_next()? {
        XmlReadEvent::StartElement {
            name,
            attributes,
            namespace,
        } => match name.local_name.as_str() {
            "null" => {
                reader.expect_end_with_name("null")?;
                Content::none()
            }
            "url" | "uri" => {
                let tag = name.local_name;
                let value = reader.read_characters()?;
                reader.expect_end_with_name(&tag)?;
                Content::from_uri(value)
            }
            "Ref" => {
                let referent = reader.read_characters()?;
                reader.expect_end_with_name("Ref")?;
                if referent != "null" {
                    state.add_content_referent_rewrite(instance_id, property_name.into(), referent);
                }
                Content::none()
            }
            _ => {
                let event = XmlReadEvent::StartElement {
                    name,
                    attributes,
                    namespace,
                };
                return Err(reader.error(DecodeErrorKind::UnexpectedXmlEvent(event)));
            }
        },
        unexpected => return Err(reader.error(DecodeErrorKind::UnexpectedXmlEvent(unexpected))),
    };
    reader.expect_end_with_name(XML_TAG_NAME)?;
    Ok(value)
}

// A ContentId type is serialized as either:
// <null></null>, which indicates an empty content value
// <url>something</url>, where 'something' is a URL to use for content.
impl XmlType for Content {
    const XML_TAG_NAME: &'static str = XML_TAG_NAME;

    fn write_xml<W: Write>(&self, writer: &mut XmlEventWriter<W>) -> Result<(), EncodeError> {
        match self.value() {
            ContentType::None => {
                writer.write(XmlWriteEvent::start_element("null"))?;
            }
            ContentType::Uri(uri) => {
                writer.write(XmlWriteEvent::start_element("url"))?;
                writer.write_string(uri)?;
            }
            _ => {
                return Err(
                    writer.error(crate::error::EncodeErrorKind::UnsupportedPropertyType(
                        rbx_dom_weak::types::VariantType::Content,
                    )),
                );
            }
        }

        writer.write(XmlWriteEvent::end_element())?;

        Ok(())
    }

    fn read_xml<R: Read>(reader: &mut XmlEventReader<R>) -> Result<Self, DecodeError> {
        let value = match reader.expect_next()? {
            XmlReadEvent::StartElement {
                name,
                attributes,
                namespace,
            } => match name.local_name.as_str() {
                "null" => {
                    reader.expect_end_with_name("null")?;

                    Content::none()
                }
                "url" => {
                    let value = reader.read_characters()?;
                    reader.expect_end_with_name("url")?;

                    Content::from_uri(value)
                }
                "uri" => {
                    let value = reader.read_characters()?;
                    reader.expect_end_with_name("uri")?;

                    Content::from_uri(value)
                }
                _ => {
                    let event = XmlReadEvent::StartElement {
                        name,
                        attributes,
                        namespace,
                    };
                    return Err(reader.error(DecodeErrorKind::UnexpectedXmlEvent(event)));
                }
            },
            unexpected => return Err(reader.error(DecodeErrorKind::UnexpectedXmlEvent(unexpected))),
        };

        Ok(value)
    }
}

/// In release 645, Roblox changed `Content` to serialize as `ContentId`.
/// At some point since then, they changed it back. We need to support this, so
/// we have a dummy value as well.
impl XmlType for ContentId {
    const XML_TAG_NAME: &'static str = "ContentId";

    fn write_xml<W: Write>(&self, writer: &mut XmlEventWriter<W>) -> Result<(), EncodeError> {
        let url: &str = self.as_ref();
        if url.is_empty() {
            writer.write(XmlWriteEvent::start_element("null"))?;
        } else {
            writer.write(XmlWriteEvent::start_element("url"))?;
            writer.write_string(url)?;
        }

        writer.write(XmlWriteEvent::end_element())?;

        Ok(())
    }

    fn read_xml<R: Read>(reader: &mut XmlEventReader<R>) -> Result<Self, DecodeError> {
        let value = match reader.expect_next()? {
            XmlReadEvent::StartElement {
                name,
                attributes,
                namespace,
            } => match name.local_name.as_str() {
                "null" => {
                    reader.expect_end_with_name("null")?;

                    ContentId::new()
                }
                "url" => {
                    let value = reader.read_characters()?;
                    reader.expect_end_with_name("url")?;

                    ContentId::from(value)
                }
                _ => {
                    let event = XmlReadEvent::StartElement {
                        name,
                        attributes,
                        namespace,
                    };
                    return Err(reader.error(DecodeErrorKind::UnexpectedXmlEvent(event)));
                }
            },
            unexpected => return Err(reader.error(DecodeErrorKind::UnexpectedXmlEvent(unexpected))),
        };

        Ok(value)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::test_util;

    #[test]
    fn round_trip_content_url() {
        test_util::test_xml_round_trip(&Content::from_uri("url://not/really/a/url"));
    }

    #[test]
    fn round_trip_content_null() {
        test_util::test_xml_round_trip(&Content::none());
    }

    #[test]
    fn deserialize_content_url() {
        test_util::test_xml_deserialize(
            r#"
                <Content name="something">
                    <url>Some URL</url>
                </Content>
            "#,
            &Content::from_uri("Some URL"),
        );
    }

    #[test]
    fn deserialize_content_uri() {
        test_util::test_xml_deserialize(
            r#"
                <Content name="something">
                    <uri>Some URL</uri>
                </Content>
            "#,
            &Content::from_uri("Some URL"),
        );
    }

    #[test]
    fn deserialize_content_null() {
        test_util::test_xml_deserialize(
            r#"
                <Content name="something">
                    <null></null>
                </Content>
            "#,
            &Content::none(),
        );
    }

    #[test]
    fn serialize_content_uri() {
        test_util::test_xml_serialize(
            r#"
                <Content name="foo">
                    <url>Some URL</url>
                </Content>
            "#,
            &Content::from_uri("Some URL"),
        );
    }

    #[test]
    fn serialize_content_null() {
        test_util::test_xml_serialize(
            r#"
                <Content name="foo">
                    <null></null>
                </Content>
            "#,
            &Content::none(),
        );
    }

    #[test]
    fn deserialize_contentid_url() {
        test_util::test_xml_deserialize(
            r#"
                <ContentId name="something">
                    <url>Some URL</url>
                </ContentId>
            "#,
            &ContentId::from("Some URL"),
        );
    }

    #[test]
    fn deserialize_contentid_null() {
        test_util::test_xml_deserialize(
            r#"
                <ContentId name="something">
                    <null></null>
                </ContentId>
            "#,
            &ContentId::new(),
        );
    }
}
