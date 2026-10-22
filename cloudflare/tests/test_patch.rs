//! Comprehensive Unit & Adversarial Tests for Runner Patch Specification, Hashing,
//! Hardware Floor Validation, Permitted Machine Types, and Rollout Classification.

use serde_json::Value;
use spot_dashboard_edge::patch::{
    classify_patch_diff, classify_rollout, classify_rollout_values, compute_canonical_patch_sha256,
    merge_patches, parse_manifest_text, validate_patch_manifest, HardwarePatch, RunnerPatch,
    FORBIDDEN_MOUNTS, PERMITTED_MACHINE_TYPES,
};

#[test]
fn test_patch_vector_fixture_exact_parity() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value =
        serde_json::from_str(fixture_str).expect("Valid patch_vector.json fixture");
    let vectors = fixture["vectors"].as_array().expect("vectors array");

    assert_eq!(vectors.len(), 6, "Must test exactly 6 canonical vectors");

    for v in vectors {
        let id = v["id"].as_str().unwrap();
        let expected_raw_sha256 = v["raw_dict_sha256"].as_str().unwrap();
        let expected_patch_sha256 = v["patch_sha256"].as_str().unwrap();
        let expected_canonical_json = v["canonical_json"].as_str().unwrap();
        let input_manifest = &v["input_manifest"];

        // 1. Direct canonical hash of input_manifest Value
        let computed_raw_sha256 = compute_canonical_patch_sha256(input_manifest)
            .expect("Raw canonical hash should succeed");
        assert_eq!(
            computed_raw_sha256, expected_raw_sha256,
            "Vector {}: direct raw hash mismatch: got {}, expected {}",
            id, computed_raw_sha256, expected_raw_sha256
        );

        // 2. Deserialize to RunnerPatch and check typed canonical JSON string and patch_sha256
        let typed_patch = serde_json::from_value::<RunnerPatch>(input_manifest.clone())
            .unwrap_or_else(|e| panic!("Vector {}: should deserialize to RunnerPatch: {}", id, e));
        let typed_canonical_json = typed_patch.to_canonical_json();
        assert_eq!(
            typed_canonical_json, expected_canonical_json,
            "Vector {}: canonical JSON mismatch:\ngot:      {}\nexpected: {}",
            id, typed_canonical_json, expected_canonical_json
        );

        let typed_sha256 = typed_patch.patch_sha256();
        assert_eq!(
            typed_sha256, expected_patch_sha256,
            "Vector {}: typed patch_sha256 mismatch: got {}, expected {}",
            id, typed_sha256, expected_patch_sha256
        );
    }
}

#[test]
fn test_exclusion_of_source_commit_and_source_branch() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let base_valid = &fixture["vectors"][1]["input_manifest"];
    let base_sha = fixture["vectors"][1]["patch_sha256"].as_str().unwrap();

    let mut with_commit = base_valid.clone();
    with_commit["source_commit"] = serde_json::json!("0123456789abcdef0123456789abcdef01234567");
    with_commit["source_branch"] = serde_json::json!("refs/heads/feature-patch-branch");

    let p = serde_json::from_value::<RunnerPatch>(with_commit.clone()).unwrap();
    assert_eq!(
        p.patch_sha256(),
        base_sha,
        "source_commit/branch must be stripped from hash"
    );

    let digest_from_val = compute_canonical_patch_sha256(&with_commit).unwrap();
    assert_eq!(
        digest_from_val, base_sha,
        "Raw canonical hash must also strip source_commit/branch"
    );
}

#[test]
fn test_hardware_optional_null_stripping_and_preservation() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let base_valid = &fixture["vectors"][1]["input_manifest"];

    // 1. Optional keys explicitly set to null are stripped
    let mut m = base_valid.clone();
    m["hardware"]["runners_count"] = Value::Null;
    m["hardware"]["runner_vcpu"] = Value::Null;
    m["hardware"]["runner_ram_gib"] = Value::Null;
    m["hardware"]["host_overhead_gib"] = Value::Null;
    m["hardware"]["queued_jobs_threshold"] = Value::Null;

    let p = serde_json::from_value::<RunnerPatch>(m.clone()).unwrap();
    let canonical = p.to_canonical_json();
    assert!(
        !canonical.contains("runners_count"),
        "runners_count must be stripped when null"
    );
    assert!(
        !canonical.contains("runner_vcpu"),
        "runner_vcpu must be stripped when null"
    );
    assert!(
        !canonical.contains("runner_ram_gib"),
        "runner_ram_gib must be stripped when null"
    );
    assert!(
        !canonical.contains("host_overhead_gib"),
        "host_overhead_gib must be stripped when null"
    );
    assert!(
        !canonical.contains("queued_jobs_threshold"),
        "queued_jobs_threshold must be stripped when null"
    );

    // 2. min_vcpu: null and min_ram_gib: null are PRESERVED
    let mut m2 = base_valid.clone();
    m2["hardware"]["min_vcpu"] = Value::Null;
    m2["hardware"]["min_ram_gib"] = Value::Null;
    let p2 = serde_json::from_value::<RunnerPatch>(m2).unwrap();
    let canonical2 = p2.to_canonical_json();
    assert!(
        canonical2.contains("\"min_vcpu\":null"),
        "min_vcpu: null must be preserved in canonical JSON"
    );
    assert!(
        canonical2.contains("\"min_ram_gib\":null"),
        "min_ram_gib: null must be preserved in canonical JSON"
    );
}

#[test]
fn test_fast_boot_golden_image_and_instance_template_omission_parity() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let v6 = &fixture["vectors"][5];
    let v6_manifest = &v6["input_manifest"];
    let expected_v6_sha = v6["patch_sha256"].as_str().unwrap();

    // v6 has golden image and template omitted; verify exact hash
    let p6 = serde_json::from_value::<RunnerPatch>(v6_manifest.clone()).unwrap();
    assert_eq!(p6.patch_sha256(), expected_v6_sha);

    // Explicitly setting them to None / null produces exact same hash
    let mut m_explicit_none = v6_manifest.clone();
    m_explicit_none["source_image"] = Value::Null;
    m_explicit_none["golden_image_family"] = Value::Null;
    m_explicit_none["source_instance_template"] = Value::Null;

    let p_none = serde_json::from_value::<RunnerPatch>(m_explicit_none.clone()).unwrap();
    assert_eq!(p_none.patch_sha256(), expected_v6_sha);
    let expected_v6_raw_sha = v6["raw_dict_sha256"].as_str().unwrap();
    assert_eq!(
        compute_canonical_patch_sha256(&m_explicit_none).unwrap(),
        expected_v6_raw_sha
    );

    // Setting source_image changes hash deterministically
    let mut m_img = v6_manifest.clone();
    m_img["source_image"] =
        serde_json::json!("projects/homelab-424523/global/images/runner-golden-20261021");
    let p_img = serde_json::from_value::<RunnerPatch>(m_img).unwrap();
    assert_ne!(p_img.patch_sha256(), expected_v6_sha);

    // Setting source_instance_template changes hash deterministically
    let mut m_tpl = v6_manifest.clone();
    m_tpl["source_instance_template"] =
        serde_json::json!("projects/homelab-424523/global/instanceTemplates/runner-template-1");
    let p_tpl = serde_json::from_value::<RunnerPatch>(m_tpl).unwrap();
    assert_ne!(p_tpl.patch_sha256(), expected_v6_sha);
}

#[test]
fn test_hardware_floor_sizing_validation_matrix() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let base_valid = &fixture["vectors"][1]["input_manifest"];

    // 1. Valid baseline passes
    assert!(validate_patch_manifest(base_valid).is_ok());

    // 2. vCPU Floor (16 passes, 15 fails)
    let mut m_vcpu_16 = base_valid.clone();
    m_vcpu_16["hardware"]["min_vcpu"] = serde_json::json!(16);
    assert!(validate_patch_manifest(&m_vcpu_16).is_ok());

    let mut m_vcpu_15 = base_valid.clone();
    m_vcpu_15["hardware"]["min_vcpu"] = serde_json::json!(15);
    let errs = validate_patch_manifest(&m_vcpu_15).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| e.contains("min_vcpu_below_floor: 15 < 16")));

    // 3. RAM Floor (32 passes, 31 fails)
    let mut m_ram_32 = base_valid.clone();
    m_ram_32["hardware"]["min_ram_gib"] = serde_json::json!(32);
    assert!(validate_patch_manifest(&m_ram_32).is_ok());

    let mut m_ram_31 = base_valid.clone();
    m_ram_31["hardware"]["min_ram_gib"] = serde_json::json!(31);
    let errs = validate_patch_manifest(&m_ram_31).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| e.contains("min_ram_gib_below_floor: 31 < 32")));

    // 4. Runner Count (2 passes, 1 and 3 fail)
    let mut m_rc_2 = base_valid.clone();
    m_rc_2["hardware"]["runners_count"] = serde_json::json!(2);
    assert!(validate_patch_manifest(&m_rc_2).is_ok());

    let mut m_rc_1 = base_valid.clone();
    m_rc_1["hardware"]["runners_count"] = serde_json::json!(1);
    let errs = validate_patch_manifest(&m_rc_1).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| e.contains("runners_count_must_be_2: got 1")));

    let mut m_rc_3 = base_valid.clone();
    m_rc_3["hardware"]["runners_count"] = serde_json::json!(3);
    let errs = validate_patch_manifest(&m_rc_3).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| e.contains("runners_count_must_be_2: got 3")));

    // 5. Runner vCPU (7 passes, 6 and 8 fail)
    let mut m_rv_7 = base_valid.clone();
    m_rv_7["hardware"]["runner_vcpu"] = serde_json::json!(7);
    assert!(validate_patch_manifest(&m_rv_7).is_ok());

    let mut m_rv_6 = base_valid.clone();
    m_rv_6["hardware"]["runner_vcpu"] = serde_json::json!(6);
    let errs = validate_patch_manifest(&m_rv_6).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| e.contains("runner_vcpu_must_be_7: got 6")));

    let mut m_rv_8 = base_valid.clone();
    m_rv_8["hardware"]["runner_vcpu"] = serde_json::json!(8);
    let errs = validate_patch_manifest(&m_rv_8).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| e.contains("runner_vcpu_must_be_7: got 8")));

    // 6. Runner RAM (14 passes, 12 and 16 fail)
    let mut m_rr_14 = base_valid.clone();
    m_rr_14["hardware"]["runner_ram_gib"] = serde_json::json!(14);
    assert!(validate_patch_manifest(&m_rr_14).is_ok());

    let mut m_rr_12 = base_valid.clone();
    m_rr_12["hardware"]["runner_ram_gib"] = serde_json::json!(12);
    let errs = validate_patch_manifest(&m_rr_12).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| e.contains("runner_ram_gib_must_be_14: got 12")));

    // 7. Host Overhead (4 passes, 5 fails)
    let mut m_ho_4 = base_valid.clone();
    m_ho_4["hardware"]["host_overhead_gib"] = serde_json::json!(4);
    assert!(validate_patch_manifest(&m_ho_4).is_ok());

    let mut m_ho_5 = base_valid.clone();
    m_ho_5["hardware"]["host_overhead_gib"] = serde_json::json!(5);
    let errs = validate_patch_manifest(&m_ho_5).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| e.contains("host_overhead_gib_must_be_4: got 5")));

    // 8. Queue Threshold (20 passes, 19 fails)
    let mut m_qj_20 = base_valid.clone();
    m_qj_20["hardware"]["queued_jobs_threshold"] = serde_json::json!(20);
    assert!(validate_patch_manifest(&m_qj_20).is_ok());

    let mut m_qj_19 = base_valid.clone();
    m_qj_19["hardware"]["queued_jobs_threshold"] = serde_json::json!(19);
    let errs = validate_patch_manifest(&m_qj_19).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| e.contains("queued_jobs_threshold_below_floor: 19 < 20")));

    // 9. Multi-Error Reporting: reports both min_vcpu and min_ram_gib
    let mut m_multi = base_valid.clone();
    m_multi["hardware"]["min_vcpu"] = serde_json::json!(4);
    m_multi["hardware"]["min_ram_gib"] = serde_json::json!(8);
    let errs = validate_patch_manifest(&m_multi).unwrap_err();
    assert!(errs.len() >= 2);
    assert!(errs.iter().any(|e| e.contains("min_vcpu_below_floor")));
    assert!(errs.iter().any(|e| e.contains("min_ram_gib_below_floor")));
}

#[test]
fn test_permitted_machine_types_allowlist() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let base_valid = &fixture["vectors"][1]["input_manifest"];

    // Permitted shapes
    for permitted in PERMITTED_MACHINE_TYPES {
        let mut m = base_valid.clone();
        m["hardware"]["extra_labels"] = serde_json::json!([format!("type-{}", permitted)]);
        assert!(
            validate_patch_manifest(&m).is_ok(),
            "Shape {} must be permitted",
            permitted
        );
    }

    // Disallowed shapes
    let disallowed = &[
        "e2-standard-4",
        "n1-standard-1",
        "c4d-standard-16",
        "c4d-highcpu-8",
        "c4d-highcpu-32",
        "C4D-HIGHCPU-16",
        "",
    ];
    for bad_shape in disallowed {
        let mut m = base_valid.clone();
        m["hardware"]["extra_labels"] = serde_json::json!([format!("type-{}", bad_shape)]);
        let errs = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("unpermitted_machine_type")),
            "Disallowed shape '{}' must be rejected",
            bad_shape
        );
    }
}

#[test]
fn test_forbidden_mount_sources_rejected() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let base_valid = &fixture["vectors"][1]["input_manifest"];

    for forbidden in FORBIDDEN_MOUNTS {
        // Exact match
        let mut m1 = base_valid.clone();
        m1["container"]["mounts"] = serde_json::json!([
            {"source": forbidden, "target": "/data"}
        ]);
        let errs1 = validate_patch_manifest(&m1).unwrap_err();
        assert!(
            errs1.iter().any(|e| e.contains("forbidden_mount_source")),
            "Exact forbidden mount '{}' must be rejected",
            forbidden
        );

        // Subpath match
        let mut m2 = base_valid.clone();
        m2["container"]["mounts"] = serde_json::json!([
            {"source": format!("{}/subpath", forbidden), "target": "/data"}
        ]);
        let errs2 = validate_patch_manifest(&m2).unwrap_err();
        assert!(
            errs2.iter().any(|e| e.contains("forbidden_mount_source")),
            "Forbidden mount subpath '{}/subpath' must be rejected",
            forbidden
        );
    }

    // Allowed mount source
    let mut m_ok = base_valid.clone();
    m_ok["container"]["mounts"] = serde_json::json!([
        {"source": "/mnt/runner-data/cache", "target": "/cache", "read_only": false}
    ]);
    assert!(validate_patch_manifest(&m_ok).is_ok());
}

#[test]
fn test_multi_repo_manifest_merging() {
    let defaults = RunnerPatch {
        schema: "spot-runner-patch-v1".to_string(),
        repository: "swack-tools".to_string(),
        hardware: HardwarePatch {
            min_vcpu: Some(16),
            min_ram_gib: Some(32),
            arch: "x86_64".to_string(),
            extra_labels: vec!["type-c4d-highcpu-16".to_string()],
            ..Default::default()
        },
        host: spot_dashboard_edge::patch::HostPatch {
            apt_packages: vec!["git".to_string(), "curl".to_string()],
            boot_script: "echo 'default boot'".to_string(),
            ..Default::default()
        },
        container: spot_dashboard_edge::patch::ContainerPatch {
            apt_packages: vec!["build-essential".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };

    let override_patch = RunnerPatch {
        repository: "xbmc4lyfe/rustnzb".to_string(),
        host: spot_dashboard_edge::patch::HostPatch {
            apt_packages: vec!["ripgrep".to_string()],
            boot_script: "echo 'repo boot'".to_string(),
            ..Default::default()
        },
        container: spot_dashboard_edge::patch::ContainerPatch {
            apt_packages: vec!["cmake".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };

    let merged = merge_patches(&defaults, &override_patch);
    assert_eq!(merged.repository, "xbmc4lyfe/rustnzb");
    assert_eq!(merged.hardware.min_vcpu, Some(16));
    assert_eq!(merged.hardware.min_ram_gib, Some(32));

    // Package lists unioned
    assert!(merged.host.apt_packages.contains(&"git".to_string()));
    assert!(merged.host.apt_packages.contains(&"curl".to_string()));
    assert!(merged.host.apt_packages.contains(&"ripgrep".to_string()));

    assert!(merged
        .container
        .apt_packages
        .contains(&"build-essential".to_string()));
    assert!(merged.container.apt_packages.contains(&"cmake".to_string()));

    // Boot scripts concatenated
    assert_eq!(
        merged.host.boot_script,
        "echo 'default boot'\necho 'repo boot'"
    );
}

#[test]
fn test_rollout_diff_classification_exhaustive() {
    let cur = spot_dashboard_edge::patch::make_test_manifest();
    let mut tgt = cur.clone();

    // 1. Identical -> noop
    assert_eq!(classify_patch_diff(&cur, &tgt), "noop");
    let c = classify_rollout(Some(&cur), &tgt);
    assert_eq!(c.action, "noop");

    // 2. Add container package -> hot_patch
    tgt.container.apt_packages.push("jq".to_string());
    assert_eq!(classify_patch_diff(&cur, &tgt), "hot_patch");
    let c = classify_rollout(Some(&cur), &tgt);
    assert_eq!(c.action, "hot_patch");

    // 3. Container dockerfile_run change -> recreate_containers
    let mut tgt_docker = cur.clone();
    tgt_docker.container.dockerfile_run = "RUN echo update".to_string();
    assert_eq!(
        classify_patch_diff(&cur, &tgt_docker),
        "recreate_containers"
    );
    let c = classify_rollout(Some(&cur), &tgt_docker);
    assert_eq!(c.action, "recreate_containers");

    // 4. Container mounts change -> recreate_containers
    let mut tgt_mount = cur.clone();
    tgt_mount
        .container
        .mounts
        .push(spot_dashboard_edge::patch::PatchMountSpec {
            source: "/mnt/runner-data/cache".to_string(),
            target: "/cache".to_string(),
            read_only: false,
        });
    assert_eq!(classify_patch_diff(&cur, &tgt_mount), "recreate_containers");
    let c = classify_rollout(Some(&cur), &tgt_mount);
    assert_eq!(c.action, "recreate_containers");

    // 5. Hardware extra_labels change -> replace_vm
    let mut tgt_hw = cur.clone();
    tgt_hw
        .hardware
        .extra_labels
        .push("type-n4-highcpu-16".to_string());
    assert_eq!(classify_patch_diff(&cur, &tgt_hw), "replace_vm");
    let c = classify_rollout(Some(&cur), &tgt_hw);
    assert_eq!(c.action, "replace_vm");

    // 6. Host boot script change -> replace_vm
    let mut tgt_boot = cur.clone();
    tgt_boot.host.boot_script = "echo 'new setup'".to_string();
    assert_eq!(classify_patch_diff(&cur, &tgt_boot), "replace_vm");
    let c = classify_rollout(Some(&cur), &tgt_boot);
    assert_eq!(c.action, "replace_vm");

    // 7. Value-based classification parity
    let cur_val = serde_json::to_value(&cur).unwrap();
    let tgt_val = serde_json::to_value(&tgt_boot).unwrap();
    let cv = classify_rollout_values(Some(&cur_val), &tgt_val);
    assert_eq!(cv.action, "replace_vm");

    // 8. Empty value manifests -> noop
    let empty_val = serde_json::json!({});
    let cv_empty = classify_rollout_values(Some(&empty_val), &empty_val);
    assert_eq!(cv_empty.action, "noop");
}

#[test]
fn test_yaml_manifest_parser() {
    let yaml = r#"
schema: spot-runner-patch-v1
repository: swack-tools/oxidex
hardware:
  arch: x86_64
  min_vcpu: 16
  min_ram_gib: 32
  runners_count: 2
  runner_vcpu: 7
  runner_ram_gib: 14
  host_overhead_gib: 4
  queued_jobs_threshold: 20
  extra_labels:
    - type-c4d-highcpu-16
host:
  apt_packages:
    - git
    - curl
  sysctl:
    net.core.somaxconn: 4096
container:
  apt_packages:
    - build-essential
"#;

    let parsed = parse_manifest_text(yaml).expect("Valid YAML should parse");
    assert_eq!(parsed.schema, "spot-runner-patch-v1");
    assert_eq!(parsed.repository, "swack-tools/oxidex");
    assert_eq!(parsed.hardware.min_vcpu, Some(16));
    assert_eq!(parsed.hardware.min_ram_gib, Some(32));
    assert_eq!(parsed.hardware.runners_count, Some(2));
    assert_eq!(parsed.host.apt_packages, vec!["git", "curl"]);
    assert_eq!(
        parsed.host.sysctl.get("net.core.somaxconn"),
        Some(&"4096".to_string())
    );
}
