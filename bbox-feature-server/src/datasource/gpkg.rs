use crate::config::{DatasourceCfg, DsGpkgCfg, GpkgCollectionCfg};
use crate::datasource::{
    AutoscanCollectionDatasource, CollectionDatasource, CollectionSource, CollectionSourceCfg,
    ConfiguredCollectionCfg, ItemsResult, NamedDatasourceCfg,
};
use crate::error::{self, Result};
use crate::filter_params::FilterParams;
use crate::inventory::FeatureCollection;
use async_trait::async_trait;
use bbox_core::ogcapi::*;
use futures::TryStreamExt;
use geozero::{geojson, wkb};
use log::warn;
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Column, Row, TypeInfo};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct GpkgDatasource {
    pool: SqlitePool,
}

#[derive(Clone, Debug)]
pub struct GpkgCollectionSource {
    ds: GpkgDatasource,
    table: String,
    geometry_column: String,
    // geometry_type_name: String,
    /// Primary key column, None if multi column key.
    pk_column: Option<String>,
}

pub struct DsGpkgHandler {
    datasources: HashMap<String, GpkgDatasource>,
    /// Default datasource
    default: Option<String>,
}

#[async_trait]
impl CollectionDatasource for DsGpkgHandler {
    fn new() -> Self {
        DsGpkgHandler {
            datasources: HashMap::new(),
            default: None,
        }
    }
    async fn add_ds(&mut self, ds_config: &NamedDatasourceCfg) {
        let DatasourceCfg::Gpkg(ref cfg) = ds_config.datasource else {
            panic!();
        };
        let ds = GpkgDatasource::from_config(cfg).await.unwrap();
        self.datasources.insert(ds_config.name.clone(), ds);
        self.default.get_or_insert(ds_config.name.clone());
    }
    async fn setup_collection(
        &mut self,
        collection: &ConfiguredCollectionCfg,
    ) -> Result<FeatureCollection> {
        let CollectionSourceCfg::Gpkg(ref srccfg) = collection.source else {
            panic!();
        };
        let no_default = "".to_string();
        let name = srccfg
            .datasource
            .as_ref()
            .unwrap_or(&self.default.as_ref().unwrap_or(&no_default));
        let ds = self.datasources.get(name).unwrap();
        collection_info(&ds, srccfg, collection, None).await
    }
}

impl GpkgDatasource {
    pub async fn from_config(cfg: &DsGpkgCfg) -> Result<Self> {
        Self::new_pool(cfg.path.as_os_str().to_str().unwrap()).await
    }
    pub async fn new_pool(gpkg: &str) -> Result<Self> {
        let conn_options = SqliteConnectOptions::new().filename(gpkg).read_only(true);
        let pool = SqlitePoolOptions::new()
            .min_connections(0)
            .max_connections(8)
            .connect_with(conn_options)
            .await?;
        Ok(GpkgDatasource { pool })
    }
}

#[async_trait]
impl AutoscanCollectionDatasource for GpkgDatasource {
    async fn collections(&self) -> Result<Vec<FeatureCollection>> {
        let mut collections = Vec::new();
        let sql = r#"
            SELECT contents.*
            FROM gpkg_contents contents
              JOIN gpkg_spatial_ref_sys refsys ON refsys.srs_id = contents.srs_id
              --JOIN gpkg_geometry_columns geom_cols ON geom_cols.table_name = contents.table_name
            WHERE data_type='features'
        "#;
        let mut rows = sqlx::query(sql).fetch(&self.pool);
        while let Some(row) = rows.try_next().await? {
            let table_name: &str = row.try_get("table_name")?;
            let id = table_name.to_string();
            let title: String = row.try_get("identifier")?;
            let extent = CoreExtent {
                spatial: Some(CoreExtentSpatial {
                    bbox: vec![vec![
                        row.try_get("min_x")?,
                        row.try_get("min_y")?,
                        row.try_get("max_x")?,
                        row.try_get("max_y")?,
                    ]],
                    crs: None,
                }),
                temporal: None,
            };
            let ds = GpkgCollectionCfg {
                datasource: None,
                table: Some(table_name.to_string()),
            };
            let coll_cfg = ConfiguredCollectionCfg {
                source: CollectionSourceCfg::Gpkg(GpkgCollectionCfg {
                    datasource: None,
                    table: Some(table_name.to_string()),
                }),
                name: id.clone(),
                title: Some(title),
                description: row.try_get("description")?,
            };
            let fc = collection_info(self, &ds, &coll_cfg, Some(extent)).await?;
            collections.push(fc);
        }
        Ok(collections)
    }
}

#[async_trait]
impl CollectionSource for GpkgCollectionSource {
    async fn items(&self, filter: &FilterParams) -> Result<ItemsResult> {
        let mut sql = format!(
            "SELECT *, count(*) OVER() AS __total_cnt FROM {table}",
            table = &self.table
        );
        if let Some(_bboxstr) = &filter.bbox {
            warn!("Ignoring bbox filter (not supported for this datasource)");
        }
        let limit = filter.limit_or_default();
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        if let Some(offset) = filter.offset {
            sql.push_str(&format!(" OFFSET {offset}"));
        }
        let rows = sqlx::query(&sql).fetch_all(&self.ds.pool).await?;
        let number_matched = if let Some(row) = rows.first() {
            row.try_get::<u32, _>("__total_cnt")? as u64
        } else {
            0
        };
        let number_returned = rows.len() as u64;
        let items = rows
            .iter()
            .map(|row| row_to_feature(row, self))
            .collect::<Result<Vec<_>>>()?;
        let result = ItemsResult {
            features: items,
            number_matched,
            number_returned,
        };
        Ok(result)
    }

    async fn item(&self, collection_id: &str, feature_id: &str) -> Result<Option<CoreFeature>> {
        let Some(pk) = &self.pk_column else {
            warn!("Ignoring error getting item for {collection_id} without single primary key");
            return Ok(None);
        };
        let sql = format!("SELECT * FROM {table} WHERE {pk} = ?", table = &self.table,);
        if let Some(row) = sqlx::query(&sql)
            .bind(feature_id)
            .fetch_optional(&self.ds.pool)
            .await?
        {
            let mut item = row_to_feature(&row, self)?;
            item.links = vec![
                ApiLink {
                    href: format!("/collections/{collection_id}/items/{feature_id}"),
                    rel: Some("self".to_string()),
                    type_: Some("application/geo+json".to_string()),
                    title: Some("this document".to_string()),
                    hreflang: None,
                    length: None,
                },
                ApiLink {
                    href: format!("/collections/{collection_id}"),
                    rel: Some("collection".to_string()),
                    type_: Some("application/geo+json".to_string()),
                    title: Some("the collection document".to_string()),
                    hreflang: None,
                    length: None,
                },
            ];
            Ok(Some(item))
        } else {
            Ok(None)
        }
    }
}

async fn collection_info(
    ds: &GpkgDatasource,
    cfg: &GpkgCollectionCfg,
    collection: &ConfiguredCollectionCfg,
    extent: Option<CoreExtent>,
) -> Result<FeatureCollection> {
    let table_name = cfg.table.clone().unwrap();
    let id = &collection.name;

    let collection = CoreCollection {
        id: id.clone(),
        title: collection.title.clone(),
        description: collection.description.clone(),
        extent,
        item_type: None,
        crs: vec![],
        links: vec![ApiLink {
            href: format!("/collections/{id}/items"),
            rel: Some("items".to_string()),
            type_: Some("application/geo+json".to_string()),
            title: collection.title.clone(),
            hreflang: None,
            length: None,
        }],
    };
    let source = table_info(ds, &table_name).await?;
    let fc = FeatureCollection {
        collection,
        source: Box::new(source),
    };
    Ok(fc)
}

async fn table_info(ds: &GpkgDatasource, table: &str) -> Result<GpkgCollectionSource> {
    // TODO: support multiple geometry columns
    let sql = r#"
        SELECT column_name, geometry_type_name,
          (SELECT COUNT(*) FROM pragma_table_info(?) ti WHERE ti.pk > 0) as pksize,
          (SELECT ti.name FROM pragma_table_info(?) ti WHERE ti.pk = 1) as pk
        FROM gpkg_geometry_columns
        WHERE table_name = ?
    "#;
    let row = sqlx::query(sql)
        .bind(table)
        .bind(table)
        .bind(table)
        .fetch_one(&ds.pool)
        .await?;
    let geometry_column: String = row.try_get("column_name")?;
    let _geometry_type_name: String = row.try_get("geometry_type_name")?;
    let pksize: u16 = row.try_get("pksize")?;
    let pk_column: Option<String> = if pksize == 1 {
        row.try_get("pk")?
    } else {
        None
    };
    Ok(GpkgCollectionSource {
        ds: ds.clone(),
        table: table.to_string(),
        geometry_column,
        pk_column,
    })
}

fn row_to_feature(row: &SqliteRow, table_info: &GpkgCollectionSource) -> Result<CoreFeature> {
    let mut id = None;
    let mut properties = json!({});
    for col in row.columns() {
        #[allow(clippy::if_same_then_else)]
        if col.name() == table_info.geometry_column {
            // Skip geometry
        } else if col.name() == "__total_cnt" {
            // Skip count
        } else if col.name() == table_info.pk_column.as_ref().unwrap_or(&"".to_string()) {
            // Get id as String
            id = match col.type_info().name() {
                "TEXT" => Some(row.try_get::<String, _>(col.ordinal())?),
                "INTEGER" => Some(row.try_get::<i64, _>(col.ordinal())?.to_string()),
                _ => None,
            }
        } else {
            properties[col.name()] = match col.type_info().name() {
                "TEXT" => json!(row.try_get::<&str, _>(col.ordinal())?),
                "INTEGER" => json!(row.try_get::<i64, _>(col.ordinal())?),
                "REAL" => json!(row.try_get::<f64, _>(col.ordinal())?),
                "DATETIME" => json!(row.try_get::<&str, _>(col.ordinal())?),
                ty => json!(format!("<{ty}>")),
            }
        }
    }
    let wkb: wkb::Decode<geojson::GeoJsonString> =
        row.try_get(table_info.geometry_column.as_str())?;
    let geom = wkb.geometry.ok_or(error::Error::GeometryFormatError)?;

    let item = CoreFeature {
        type_: "Feature".to_string(),
        id,
        geometry: serde_json::from_str(&geom.0).map_err(|_| error::Error::GeometryFormatError)?,
        properties: Some(properties),
        links: vec![],
    };

    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gpkg_content() {
        let pool = GpkgDatasource::new_pool("../assets/ne_extracts.gpkg")
            .await
            .unwrap();
        let collections = pool.collections().await.unwrap();
        assert_eq!(collections.len(), 3);
        assert_eq!(
            collections
                .iter()
                .map(|col| col.collection.id.clone())
                .collect::<Vec<_>>(),
            vec![
                "ne_10m_rivers_lake_centerlines",
                "ne_10m_lakes",
                "ne_10m_populated_places"
            ]
        );
    }

    #[tokio::test]
    async fn gpkg_features() {
        let filter = FilterParams::default();
        let ds = GpkgDatasource::new_pool("../assets/ne_extracts.gpkg")
            .await
            .unwrap();
        let source = GpkgCollectionSource {
            ds,
            table: "ne_10m_lakes".to_string(),
            geometry_column: "geom".to_string(),
            pk_column: Some("fid".to_string()),
        };
        let items = source.items(&filter).await.unwrap();
        assert_eq!(items.features.len(), filter.limit_or_default() as usize);
    }
}
