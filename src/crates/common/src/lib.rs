//! Shared types: the job spec, its validation, and the Nix generator.

mod render;
mod spec;
mod validate;

pub use render::{RenderedJob, render};
pub use spec::{JobSpec, NixpkgsPin, SCHEMA_VERSION, Source};
pub use validate::{
    MAX_ENV_VALUE_BYTES, MAX_ENV_VARS, MAX_NAME_LEN, MAX_PACKAGES, MAX_SCRIPT_BYTES, MAX_URL_LEN,
};

#[cfg(test)]
pub(crate) mod testutil {
    use crate::spec::JobSpec;

    pub fn hello() -> JobSpec {
        JobSpec::from_yaml(include_str!("../../../examples/hello-c/job.yaml")).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::hello;

    #[test]
    fn parses_hello_c() {
        let spec = hello();
        assert_eq!(spec.name, "hello-c");
        assert_eq!(spec.schema_version, 1);
        assert_eq!(spec.native_build_inputs, vec!["gnumake"]);
        assert!(spec.build_inputs.is_empty() && spec.env.is_empty());
        assert!(matches!(spec.src, Source::Git { .. }));
    }

    #[test]
    fn rejects_unknown_and_missing_fields() {
        let base = include_str!("../../../examples/hello-c/job.yaml");
        let typo = base.replace("native_build_inputs:", "native_build_input:");
        assert!(JobSpec::from_yaml(&typo).is_err());
        assert!(JobSpec::from_yaml(&format!("{base}bogus: 1\n")).is_err());
        let missing = base.replace("name: hello-c\n", "");
        assert!(JobSpec::from_yaml(&missing).is_err());
    }

    #[test]
    fn optional_fields_default() {
        let minimal =
            include_str!("../../../examples/hello-c/job.yaml").replace("schema_version: 1\n", "");
        let s = JobSpec::from_yaml(&minimal).unwrap();
        assert_eq!(s.schema_version, SCHEMA_VERSION);
        assert!(s.configure.is_empty() && s.check.is_empty());
    }
}
