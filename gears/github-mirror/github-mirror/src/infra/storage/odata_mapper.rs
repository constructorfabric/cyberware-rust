//! Field mappings for the `/github-mirror/v1` listings that accept `OData`.
//!
//! Each enum is the set of columns a caller may sort, page and filter on;
//! `paginate_odata` needs it to translate `$filter`/`$orderby` into SQL and to
//! build the keyset cursor.

use toolkit_db::odata::sea_orm_filter::{FieldToColumn, ODataFieldMapping};
use toolkit_odata::filter::{FieldKind, FilterField};

use super::entity::commit_files::{
    Column as CommitFileColumn, Entity as CommitFileEntity, Model as CommitFileModel,
};
use super::entity::repositories::{Column as RepoColumn, Entity as RepoEntity, Model as RepoModel};
use super::entity::review_threads::{
    Column as ReviewThreadColumn, Entity as ReviewThreadEntity, Model as ReviewThreadModel,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::VariantArray)]
pub enum RepoField {
    FullName,
    Owner,
    Name,
    Private,
    Stars,
    Forks,
}

impl FilterField for RepoField {
    const FIELDS: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;

    fn name(&self) -> &'static str {
        match self {
            Self::FullName => "full_name",
            Self::Owner => "owner",
            Self::Name => "name",
            Self::Private => "private",
            Self::Stars => "stars",
            Self::Forks => "forks",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::FullName | Self::Owner | Self::Name => FieldKind::String,
            Self::Private => FieldKind::Bool,
            Self::Stars | Self::Forks => FieldKind::I64,
        }
    }
}

pub struct RepoODataMapper;

impl FieldToColumn<RepoField> for RepoODataMapper {
    type Column = RepoColumn;

    fn map_field(field: RepoField) -> RepoColumn {
        match field {
            RepoField::FullName => RepoColumn::FullName,
            RepoField::Owner => RepoColumn::Owner,
            RepoField::Name => RepoColumn::Name,
            RepoField::Private => RepoColumn::Private,
            RepoField::Stars => RepoColumn::Stars,
            RepoField::Forks => RepoColumn::Forks,
        }
    }
}

impl ODataFieldMapping<RepoField> for RepoODataMapper {
    type Entity = RepoEntity;

    fn extract_cursor_value(model: &RepoModel, field: RepoField) -> sea_orm::Value {
        match field {
            RepoField::FullName => sea_orm::Value::String(Some(model.full_name.clone())),
            RepoField::Owner => sea_orm::Value::String(Some(model.owner.clone())),
            RepoField::Name => sea_orm::Value::String(Some(model.name.clone())),
            RepoField::Private => sea_orm::Value::Bool(Some(model.private)),
            RepoField::Stars => sea_orm::Value::BigInt(Some(model.stars)),
            RepoField::Forks => sea_orm::Value::BigInt(Some(model.forks)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::VariantArray)]
pub enum CommitFileField {
    Filename,
    Status,
    Additions,
    Deletions,
    Changes,
}

impl FilterField for CommitFileField {
    const FIELDS: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;

    fn name(&self) -> &'static str {
        match self {
            Self::Filename => "filename",
            Self::Status => "status",
            Self::Additions => "additions",
            Self::Deletions => "deletions",
            Self::Changes => "changes",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Filename | Self::Status => FieldKind::String,
            Self::Additions | Self::Deletions | Self::Changes => FieldKind::I64,
        }
    }
}

pub struct CommitFileODataMapper;

impl FieldToColumn<CommitFileField> for CommitFileODataMapper {
    type Column = CommitFileColumn;

    fn map_field(field: CommitFileField) -> CommitFileColumn {
        match field {
            CommitFileField::Filename => CommitFileColumn::Filename,
            CommitFileField::Status => CommitFileColumn::Status,
            CommitFileField::Additions => CommitFileColumn::Additions,
            CommitFileField::Deletions => CommitFileColumn::Deletions,
            CommitFileField::Changes => CommitFileColumn::Changes,
        }
    }
}

impl ODataFieldMapping<CommitFileField> for CommitFileODataMapper {
    type Entity = CommitFileEntity;

    fn extract_cursor_value(model: &CommitFileModel, field: CommitFileField) -> sea_orm::Value {
        match field {
            CommitFileField::Filename => sea_orm::Value::String(Some(model.filename.clone())),
            CommitFileField::Status => sea_orm::Value::String(Some(model.status.clone())),
            CommitFileField::Additions => sea_orm::Value::BigInt(Some(model.additions)),
            CommitFileField::Deletions => sea_orm::Value::BigInt(Some(model.deletions)),
            CommitFileField::Changes => sea_orm::Value::BigInt(Some(model.changes)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::VariantArray)]
pub enum ReviewThreadField {
    Id,
    IsResolved,
    IsOutdated,
    Path,
    CommentsCount,
}

impl FilterField for ReviewThreadField {
    const FIELDS: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;

    fn name(&self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::IsResolved => "is_resolved",
            Self::IsOutdated => "is_outdated",
            Self::Path => "path",
            Self::CommentsCount => "comments_count",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Id | Self::Path => FieldKind::String,
            Self::IsResolved | Self::IsOutdated => FieldKind::Bool,
            Self::CommentsCount => FieldKind::I64,
        }
    }
}

pub struct ReviewThreadODataMapper;

impl FieldToColumn<ReviewThreadField> for ReviewThreadODataMapper {
    type Column = ReviewThreadColumn;

    fn map_field(field: ReviewThreadField) -> ReviewThreadColumn {
        match field {
            ReviewThreadField::Id => ReviewThreadColumn::Id,
            ReviewThreadField::IsResolved => ReviewThreadColumn::IsResolved,
            ReviewThreadField::IsOutdated => ReviewThreadColumn::IsOutdated,
            ReviewThreadField::Path => ReviewThreadColumn::Path,
            ReviewThreadField::CommentsCount => ReviewThreadColumn::CommentsCount,
        }
    }
}

impl ODataFieldMapping<ReviewThreadField> for ReviewThreadODataMapper {
    type Entity = ReviewThreadEntity;

    fn extract_cursor_value(model: &ReviewThreadModel, field: ReviewThreadField) -> sea_orm::Value {
        match field {
            ReviewThreadField::Id => sea_orm::Value::String(Some(model.id.clone())),
            ReviewThreadField::IsResolved => sea_orm::Value::Bool(Some(model.is_resolved)),
            ReviewThreadField::IsOutdated => sea_orm::Value::Bool(Some(model.is_outdated)),
            ReviewThreadField::Path => sea_orm::Value::String(model.path.clone()),
            ReviewThreadField::CommentsCount => sea_orm::Value::BigInt(Some(model.comments_count)),
        }
    }
}
