//! Experimental PyPI resolution with `resolvo-uv` instead of uv's resolver.
//!
//! Selected with `PIXI_PYPI_RESOLVER`:
//!
//! - `resolvo`: conda is already solved; its records are locked and PyPI
//!   packages they install are satisfied by the conda record (the same
//!   semantics as the uv path, via [`resolvo_uv::joint`]'s bridge).
//! - `resolvo-joint`: conda and PyPI are solved together. The conda-only
//!   solve is used as the preferred solution, but PyPI requirements may
//!   move conda packages that also provide PyPI distributions.
//!
//! Metadata, index access and sdist builds reuse pixi's uv setup; only the
//! resolver differs. URL, git, path and editable PyPI requirements and
//! dependency overrides are not supported yet.

use std::{path::Path, str::FromStr, sync::Arc};

use miette::{Context, IntoDiagnostic};
use pixi_install_pypi::{LockedPypiRecord, UnresolvedPypiRecord};
use pixi_record::PixiRecord;
use pixi_uv_conversions::{
    convert_uv_requirements_to_pep508, to_uv_normalize, to_uv_version, to_version_specifiers,
};
use rattler_conda_types::{
    GenericVirtualPackage, MatchSpec, ParseStrictness, RepoDataRecord,
    package::DistArchiveIdentifier,
};
use rattler_lock::{PypiDistributionData, PypiPackageData, UrlOrPath, Verbatim};
use resolvo_uv::{
    ChosenDist, PypiClient, PypiProvider, SdistFile, SdistMetadataSource,
    joint::{self, CondaInput},
};
use uv_client::RegistryClient;
use uv_distribution::DistributionDatabase;
use uv_distribution_types::{Dist, HashPolicy, IndexCapabilities, RequirementSource};
use uv_pep508::MarkerEnvironment;
use uv_platform_tags::Tags;
use uv_pypi_types::{ResolutionMetadata, VerbatimParsedUrl};
use uv_types::BuildContext;

use super::{get_url_or_path, parse_hashes_from_hash_vec};
use crate::lock_file::{LockedPypiRecords, PypiPackageIdentifier, records_by_name::HasNameVersion};

/// Which resolver `resolve_pypi` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PypiResolver {
    Uv,
    Resolvo,
    ResolvoJoint,
}

impl PypiResolver {
    pub fn from_env() -> Self {
        match std::env::var("PIXI_PYPI_RESOLVER").as_deref() {
            Ok("resolvo") => Self::Resolvo,
            Ok("resolvo-joint") => Self::ResolvoJoint,
            _ => Self::Uv,
        }
    }
}

/// What a joint solve needs to re-solve the conda side of an environment.
pub struct JointSetup {
    pub channels: Vec<rattler_conda_types::ChannelUrl>,
    pub platform: rattler_conda_types::Platform,
    /// The binary conda dependencies of the environment.
    pub specs: Vec<MatchSpec>,
    pub constraints: Vec<MatchSpec>,
    pub channel_priority: rattler_solve::ChannelPriority,
    pub strategy: rattler_solve::SolveStrategy,
    pub mapping_client: pypi_mapping::PurlDerivationClient,
    pub derivation_mode: pypi_mapping::PurlDerivationMode,
}

/// The conda candidates of a joint solve.
struct JointConda {
    /// Every record the conda side may choose from.
    records: Vec<RepoDataRecord>,
    specs: Vec<MatchSpec>,
    constraints: Vec<MatchSpec>,
    channel_priority: rattler_solve::ChannelPriority,
    strategy: rattler_solve::SolveStrategy,
}

/// Fetch the conda candidates for a joint solve and derive purls for the
/// records that can matter to PyPI.
///
/// Deriving purls costs a (cached) lookup per record, so only records of
/// conda packages that already install a PyPI distribution in the conda-only
/// solution are mapped. Other conda packages cannot bridge to PyPI.
async fn prepare_joint_conda(
    setup: &JointSetup,
    gateway: &rattler_repodata_gateway::Gateway,
    presolved: &[RepoDataRecord],
) -> miette::Result<JointConda> {
    let python = presolved
        .iter()
        .find(|r| r.package_record.name.as_normalized() == "python")
        .ok_or_else(|| {
            miette::miette!("a joint solve requires python in the conda dependencies")
        })?;

    let repodata = gateway
        .query(
            setup
                .channels
                .iter()
                .cloned()
                .map(rattler_conda_types::Channel::from_url),
            [setup.platform, rattler_conda_types::Platform::NoArch],
            setup.specs.iter().chain(&setup.constraints).cloned(),
        )
        .recursive(true)
        .await
        .into_diagnostic()
        .context("failed to fetch conda repodata for the joint solve")?
        .repodata;
    let mut records: Vec<RepoDataRecord> =
        repodata.iter().flat_map(|r| r.iter().cloned()).collect();

    let bridged_names: std::collections::HashSet<_> = presolved
        .iter()
        .filter(|r| !pypi_packages_provided_by(r).is_empty())
        .map(|r| r.package_record.name.clone())
        .collect();
    setup
        .mapping_client
        .amend_purls(
            &setup.derivation_mode,
            records
                .iter_mut()
                .filter(|r| bridged_names.contains(&r.package_record.name)),
            None,
        )
        .await?;
    tracing::info!(
        "joint solve: {} conda candidates, {} mapped to pypi",
        records.len(),
        records
            .iter()
            .filter(|r| bridged_names.contains(&r.package_record.name))
            .count()
    );

    // Marker evaluation and wheel tags need a concrete interpreter, so the
    // conda-only solve's Python stays fixed.
    let python_pin = MatchSpec::from_str(
        &format!("python =={}", python.package_record.version),
        ParseStrictness::Lenient,
    )
    .into_diagnostic()?;

    Ok(JointConda {
        records,
        specs: setup.specs.clone(),
        constraints: setup
            .constraints
            .iter()
            .cloned()
            .chain(std::iter::once(python_pin))
            .collect(),
        channel_priority: setup.channel_priority,
        strategy: setup.strategy,
    })
}

/// Builds sdist metadata with pixi's build dispatch (conda prefix, build
/// isolation settings, config settings).
struct PixiSdistSource<'a, Context: BuildContext> {
    database: DistributionDatabase<'a, Context>,
}

impl<Context: BuildContext> SdistMetadataSource for PixiSdistSource<'_, Context> {
    fn sdist_metadata<'s>(
        &'s self,
        sdist: &'s SdistFile,
    ) -> futures::future::LocalBoxFuture<
        's,
        Result<Option<ResolutionMetadata>, Box<dyn std::error::Error>>,
    > {
        Box::pin(async move {
            let dist = Dist::Source(sdist.to_source_dist());
            match self
                .database
                .get_or_build_wheel_metadata(&dist, HashPolicy::None)
                .await
            {
                Ok(archive) => Ok(Some(resolvo_uv::metadata::resolution_metadata_from(
                    archive.metadata,
                ))),
                Err(err) => {
                    tracing::warn!("failed to build metadata for {}: {err}", sdist.filename);
                    Ok(None)
                }
            }
        })
    }
}

/// Stand-in for a conda record that has no repodata entry (pixi source
/// packages), so it can take part in the conda side of the solve.
fn synthesize_repodata_record(record: &PixiRecord) -> miette::Result<RepoDataRecord> {
    match record {
        PixiRecord::Binary(record) => Ok((**record).clone()),
        PixiRecord::Source(source) => {
            let package_record = source.package_record().clone();
            let filename = format!(
                "{}-{}-{}.conda",
                package_record.name.as_normalized(),
                package_record.version,
                package_record.build
            );
            Ok(RepoDataRecord {
                identifier: DistArchiveIdentifier::from_str(&filename)
                    .map_err(|e| miette::miette!("{e}"))?,
                url: format!("https://pixi-source-package.invalid/{filename}")
                    .parse()
                    .into_diagnostic()?,
                channel: None,
                package_record,
            })
        }
    }
}

fn pypi_packages_provided_by(
    record: &RepoDataRecord,
) -> Vec<(uv_normalize::PackageName, uv_pep440::Version)> {
    let identifiers = match PypiPackageIdentifier::from_repodata_record(record) {
        Ok(identifiers) => identifiers,
        Err(err) => {
            tracing::debug!(
                "ignoring purls of {}: {err}",
                record.package_record.name.as_source()
            );
            return Vec::new();
        }
    };
    identifiers
        .into_iter()
        .filter_map(|id| {
            Some((
                to_uv_normalize(&id.name.as_normalized().clone()).ok()?,
                to_uv_version(&id.version).ok()?,
            ))
        })
        .collect()
}

fn to_pep508_requirements(
    requirements: &[uv_distribution_types::Requirement],
) -> miette::Result<Vec<uv_pep508::Requirement<VerbatimParsedUrl>>> {
    requirements
        .iter()
        .map(|req| match &req.source {
            RequirementSource::Registry { index, .. } => {
                if index.is_some() {
                    tracing::warn!(
                        "the resolvo resolver ignores the index pinned for '{}'",
                        req.name
                    );
                }
                Ok(req.clone().into())
            }
            _ => Err(miette::miette!(
                help = "unset PIXI_PYPI_RESOLVER to use the uv resolver",
                "the resolvo resolver only supports registry requirements, but '{}' is {}",
                req.name,
                req.source
            )),
        })
        .collect()
}

fn lock_record(
    metadata: &ResolutionMetadata,
    location: UrlOrPath,
    hash: Option<rattler_lock::PackageHashes>,
    index_url: Option<url::Url>,
) -> miette::Result<LockedPypiRecord> {
    let version = pep440_rs::Version::from_str(&metadata.version.to_string())
        .into_diagnostic()
        .context("cannot convert version")?;
    Ok(
        UnresolvedPypiRecord::from(PypiPackageData::Distribution(Box::new(
            PypiDistributionData {
                name: pep508_rs::PackageName::new(metadata.name.to_string())
                    .into_diagnostic()
                    .context("cannot convert name")?,
                hash,
                index_url,
                location: Verbatim::new(location),
                version: version.clone(),
                requires_python: metadata
                    .requires_python
                    .as_ref()
                    .map(to_version_specifiers)
                    .transpose()
                    .into_diagnostic()?,
                requires_dist: convert_uv_requirements_to_pep508(metadata.requires_dist.iter())
                    .into_diagnostic()?,
            },
        )))
        .lock(version),
    )
}

pub struct ResolvoOutcome {
    pub pypi: LockedPypiRecords,
    /// The conda solution of a joint solve (`None` for the sequential mode,
    /// where the conda records are fixed).
    pub conda: Option<Vec<RepoDataRecord>>,
}

#[allow(clippy::too_many_arguments)]
pub async fn resolve<Context: BuildContext>(
    requirements: &[uv_distribution_types::Requirement],
    has_dependency_overrides: bool,
    locked_pixi_records: &[PixiRecord],
    locked_pypi_packages: &[UnresolvedPypiRecord],
    virtual_packages: Vec<GenericVirtualPackage>,
    joint: Option<(&JointSetup, &rattler_repodata_gateway::Gateway)>,
    marker_environment: &MarkerEnvironment,
    tags: &Tags,
    registry_client: &Arc<RegistryClient>,
    capabilities: &IndexCapabilities,
    downloads_semaphore: Arc<tokio::sync::Semaphore>,
    build_context: &Context,
    project_root: &Path,
) -> miette::Result<ResolvoOutcome> {
    if has_dependency_overrides {
        miette::bail!(
            help = "unset PIXI_PYPI_RESOLVER to use the uv resolver",
            "the resolvo resolver does not support `dependency-overrides` yet"
        );
    }
    let pypi_requirements = to_pep508_requirements(requirements)?;

    let presolved = locked_pixi_records
        .iter()
        .map(synthesize_repodata_record)
        .collect::<miette::Result<Vec<_>>>()?;
    let presolved_refs: Vec<&RepoDataRecord> = presolved.iter().collect();

    let joint = match joint {
        Some(_)
            if locked_pixi_records
                .iter()
                .any(|r| matches!(r, PixiRecord::Source(_))) =>
        {
            tracing::warn!(
                "joint conda/pypi solves do not support conda source packages yet; \
                 keeping the conda solution fixed"
            );
            None
        }
        joint => joint,
    };
    let joint_conda = match joint {
        Some((setup, gateway)) => Some(prepare_joint_conda(setup, gateway, &presolved).await?),
        None => None,
    };

    let name_spec = |record: &RepoDataRecord| {
        MatchSpec::from_str(
            record.package_record.name.as_normalized(),
            ParseStrictness::Lenient,
        )
        .into_diagnostic()
    };
    let conda_input = match &joint_conda {
        // Sequential: the conda solution is fixed. Requiring every record
        // keeps them all in the model, so a PyPI package they install can
        // only be satisfied by the conda record.
        None => CondaInput {
            records: &[],
            favored: &[],
            locked: &presolved_refs,
            virtual_packages: &virtual_packages,
            specs: presolved
                .iter()
                .map(name_spec)
                .collect::<miette::Result<_>>()?,
            constraints: Vec::new(),
            channel_priority: rattler_solve::ChannelPriority::Strict,
            strategy: rattler_solve::SolveStrategy::Highest,
        },
        Some(joint) => CondaInput {
            records: &joint.records,
            favored: &presolved_refs,
            locked: &[],
            virtual_packages: &virtual_packages,
            specs: joint.specs.clone(),
            constraints: joint.constraints.clone(),
            channel_priority: joint.channel_priority,
            strategy: joint.strategy,
        },
    };

    let preferences = locked_pypi_packages
        .iter()
        .filter_map(|record| {
            let version = record.version()?;
            Some((
                to_uv_normalize(record.name()).ok()?,
                to_uv_version(version).ok()?,
            ))
        })
        .collect::<Vec<_>>();

    let client = Arc::new(PypiClient::from_registry(
        Arc::clone(registry_client),
        capabilities.clone(),
        Arc::clone(&downloads_semaphore),
    ));
    let sdist_source = PixiSdistSource {
        database: DistributionDatabase::new(registry_client, build_context, downloads_semaphore),
    };
    let provider = PypiProvider::new(client, marker_environment.clone(), tags.clone())
        .with_sdist_source(&sdist_source)
        .with_preferences(preferences);

    // resolvo's solve is synchronous and drives the provider's futures by
    // blocking on the runtime, which is only allowed outside of it.
    let runtime = tokio::runtime::Handle::current();
    let start = std::time::Instant::now();
    let solution = tokio::task::block_in_place(|| {
        joint::solve(
            &conda_input,
            provider,
            &pypi_requirements,
            &pypi_packages_provided_by,
            runtime,
        )
    })
    .map_err(|err| match err {
        joint::JointSolveError::Unsolvable(message) => {
            miette::miette!("failed to resolve pypi dependencies:\n{message}")
        }
        err => miette::miette!("{err}"),
    })?;
    tracing::info!(
        "resolvo resolved {} pypi packages ({} provided by conda) in {:?}",
        solution.pypi_packages.len(),
        solution.pypi_from_conda.len(),
        start.elapsed()
    );

    let cache = &solution.pypi_cache;
    let mut locked = Vec::with_capacity(solution.pypi_packages.len());
    for (name, version) in &solution.pypi_packages {
        let metadata = cache.cached_metadata(name, version).ok_or_else(|| {
            miette::miette!("resolvo resolved {name}=={version} without its metadata")
        })?;
        let dist = cache
            .best_distribution(name, version)
            .ok_or_else(|| miette::miette!("no installable distribution for {name}=={version}"))?;
        let record = match dist {
            ChosenDist::Wheel(wheel) => lock_record(
                &metadata,
                get_url_or_path(&wheel.index, &wheel.file.url, project_root).into_diagnostic()?,
                parse_hashes_from_hash_vec(&wheel.file.hashes).into_diagnostic()?,
                Some((*wheel.index).clone()),
            )?,
            ChosenDist::Sdist(sdist) => lock_record(
                &metadata,
                get_url_or_path(&sdist.index, &sdist.file.url, project_root).into_diagnostic()?,
                parse_hashes_from_hash_vec(&sdist.file.hashes).into_diagnostic()?,
                Some((*sdist.index).clone()),
            )?,
        };
        locked.push(record);
    }

    let conda = match (joint, joint_conda) {
        (Some((setup, _)), Some(_)) => {
            let mut records = solution.conda_records;
            // Records outside the mapped subset have no purls yet; the lock
            // file stores them for every conda record.
            setup
                .mapping_client
                .amend_purls(
                    &setup.derivation_mode,
                    records
                        .iter_mut()
                        .filter(|r| r.package_record.purls.is_none()),
                    None,
                )
                .await?;
            Some(records)
        }
        _ => None,
    };
    Ok(ResolvoOutcome {
        pypi: locked,
        conda,
    })
}
