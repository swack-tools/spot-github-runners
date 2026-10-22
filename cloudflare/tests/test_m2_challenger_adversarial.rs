//! Milestone 2 Adversarial Challenger Test Suite
//!
//! Empirically challenges:
//! 1. Exact bit-for-bit SHA-256 match for all 6 test vectors from `tests/fixtures/patch_vector.json`.
//! 2. Adversarial key ordering permutations at root, hardware, host, container, and nested object levels.
//! 3. Null values and omission oracle (stripping vs preservation of min_vcpu/min_ram_gib nulls).
//! 4. Comprehensive Unicode and Python `ensure_ascii=True` escaping oracle (control chars, DEL, Latin-1, CJK, RTL, UTF-16 surrogate pairs for SMP/emojis).
//! 5. Multiline boot scripts and file contents (newlines, carriage returns, quotes, heredocs, escapes).
//! 6. Fast-boot golden image and instance template additive parity & omission oracle.
//! 7. Hardware floor sizing boundary conditions and permitted machine types.
//! 8. Forbidden mounts and subpath traversal rejection.
//! 9. Multi-repo merge and rollout diff classification oracle.

use serde_json::{json, Value};
use spot_dashboard_edge::patch::{
    classify_patch_diff, classify_rollout, compute_canonical_patch_sha256, merge_patches,
    to_canonical_json_str, validate_patch_manifest, write_canonical_json_value, RunnerPatch,
    FORBIDDEN_MOUNTS, PERMITTED_MACHINE_TYPES,
};

#[test]
fn test_vector_fixtures_all_6_exact_match() {
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

        // 1. Direct raw canonical hash of input_manifest Value
        let computed_raw_sha256 = compute_canonical_patch_sha256(input_manifest)
            .expect("Raw canonical hash must succeed");
        assert_eq!(
            computed_raw_sha256, expected_raw_sha256,
            "Vector {}: direct raw hash mismatch: got {}, expected {}",
            id, computed_raw_sha256, expected_raw_sha256
        );

        // 2. Typed canonical JSON string and patch_sha256
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
fn test_adversarial_key_ordering_permutations_oracle() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let v2_manifest = &fixture["vectors"][1]["input_manifest"];
    let expected_sha = fixture["vectors"][1]["patch_sha256"].as_str().unwrap();

    // Construct 5 different dictionary insertion orderings for the exact same data
    let orders: Vec<Vec<&str>> = vec![
        vec!["schema", "repository", "hardware", "host", "container"],
        vec!["container", "host", "hardware", "repository", "schema"],
        vec!["hardware", "container", "schema", "host", "repository"],
        vec!["repository", "schema", "host", "container", "hardware"],
        vec!["host", "hardware", "container", "repository", "schema"],
    ];

    for (idx, order) in orders.iter().enumerate() {
        let mut permuted_obj = serde_json::Map::new();
        for key in order {
            if let Some(val) = v2_manifest.get(*key) {
                // If it's an object (hardware, host, container), also reverse its keys
                if let Value::Object(sub_map) = val {
                    let mut reversed_sub = serde_json::Map::new();
                    let mut keys: Vec<&String> = sub_map.keys().collect();
                    if idx % 2 == 1 {
                        keys.reverse();
                    }
                    for sub_k in keys {
                        reversed_sub.insert(sub_k.clone(), sub_map[sub_k].clone());
                    }
                    permuted_obj.insert(key.to_string(), Value::Object(reversed_sub));
                } else {
                    permuted_obj.insert(key.to_string(), val.clone());
                }
            }
        }

        let permuted_val = Value::Object(permuted_obj);

        // 1. Raw canonical hash must match expected bit-for-bit
        let computed_raw_sha = compute_canonical_patch_sha256(&permuted_val).unwrap();
        assert_eq!(
            computed_raw_sha, expected_sha,
            "Permutation {}: raw canonical hash mismatch",
            idx
        );

        // 2. Typed patch must match expected bit-for-bit
        let p = serde_json::from_value::<RunnerPatch>(permuted_val).unwrap();
        assert_eq!(
            p.patch_sha256(),
            expected_sha,
            "Permutation {}: typed patch_sha256 mismatch",
            idx
        );
    }
}

#[test]
fn test_adversarial_null_values_and_stripping_oracle() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let v2_manifest = fixture["vectors"][1]["input_manifest"].clone();

    // 1. Hardware optional fields: omitting vs setting to null
    let optional_hw_keys = [
        "runners_count",
        "runner_vcpu",
        "runner_ram_gib",
        "host_overhead_gib",
        "queued_jobs_threshold",
    ];

    let mut m_omitted = v2_manifest.clone();
    let mut m_null = v2_manifest.clone();

    for k in &optional_hw_keys {
        m_omitted["hardware"].as_object_mut().unwrap().remove(*k);
        m_null["hardware"][k] = Value::Null;
    }

    let p_omitted = serde_json::from_value::<RunnerPatch>(m_omitted.clone()).unwrap();
    let p_null = serde_json::from_value::<RunnerPatch>(m_null.clone()).unwrap();

    assert_eq!(
        p_omitted.patch_sha256(),
        p_null.patch_sha256(),
        "Omitting optional hardware fields must produce identical digest to setting them to null"
    );

    let raw_omitted = compute_canonical_patch_sha256(&m_omitted).unwrap();
    let raw_null = compute_canonical_patch_sha256(&m_null).unwrap();
    assert_eq!(
        raw_omitted, raw_null,
        "Raw canonical hash for omitted vs null optional hardware fields must match"
    );

    // 2. min_vcpu: null and min_ram_gib: null must be strictly preserved
    let mut m_pres = v2_manifest.clone();
    m_pres["hardware"]["min_vcpu"] = Value::Null;
    m_pres["hardware"]["min_ram_gib"] = Value::Null;
    let p_pres = serde_json::from_value::<RunnerPatch>(m_pres.clone()).unwrap();
    let canon_pres = p_pres.to_canonical_json();
    assert!(
        canon_pres.contains("\"min_ram_gib\":null"),
        "min_ram_gib: null must be present in canonical JSON"
    );
    assert!(
        canon_pres.contains("\"min_vcpu\":null"),
        "min_vcpu: null must be present in canonical JSON"
    );

    // 3. source_commit and source_branch null or non-null must be stripped
    let mut m_src = v2_manifest.clone();
    m_src["source_commit"] = Value::Null;
    m_src["source_branch"] = Value::Null;
    let p_src1 = serde_json::from_value::<RunnerPatch>(m_src.clone()).unwrap();
    assert_eq!(
        p_src1.patch_sha256(),
        fixture["vectors"][1]["patch_sha256"].as_str().unwrap()
    );

    m_src["source_commit"] = json!("deadbeef12345678");
    m_src["source_branch"] = json!("refs/heads/main");
    let p_src2 = serde_json::from_value::<RunnerPatch>(m_src.clone()).unwrap();
    assert_eq!(
        p_src2.patch_sha256(),
        fixture["vectors"][1]["patch_sha256"].as_str().unwrap()
    );
}

#[test]
fn test_adversarial_unicode_and_surrogates_oracle() {
    // Test write_canonical_json_value against known Python json.dumps(..., ensure_ascii=True) outputs
    let test_cases: Vec<(&str, &str)> = vec![
        // Control characters
        ("\x00", "\"\\u0000\""),
        ("\x01", "\"\\u0001\""),
        ("\x08", "\"\\b\""),
        ("\x09", "\"\\t\""),
        ("\x0a", "\"\\n\""),
        ("\x0c", "\"\\f\""),
        ("\x0d", "\"\\r\""),
        ("\x1f", "\"\\u001f\""),
        // Quotes and backslashes
        ("\"", "\"\\\"\""),
        ("\\", "\"\\\\\""),
        // Printable ASCII
        ("hello world", "\"hello world\""),
        ("~", "\"~\""),
        // DEL character
        ("\x7f", "\"\\u007f\""),
        // Latin-1 characters
        (
            "\u{00fc}\u{00f1}\u{00ee}\u{00e7}\u{00f8}d\u{00e9}",
            "\"\\u00fc\\u00f1\\u00ee\\u00e7\\u00f8d\\u00e9\"",
        ),
        ("\u{00a9} 2026", "\"\\u00a9 2026\""),
        // CJK characters
        ("\u{4e16}\u{754c}", "\"\\u4e16\\u754c\""),
        // Hebrew / Arabic RTL
        (
            "\u{0645}\u{0631}\u{062d}\u{0628}\u{0627}",
            "\"\\u0645\\u0631\\u062d\\u0628\\u0627\"",
        ),
        // Math symbols
        ("\u{221e} \u{2192} \u{2200}", "\"\\u221e \\u2192 \\u2200\""),
        // SMP / Astral Code Points (> 0xFFFF) requiring UTF-16 surrogate pairs
        ("\u{1F680}", "\"\\ud83d\\ude80\""), // Rocket emoji
        ("\u{1F525}", "\"\\ud83d\\udd25\""), // Fire emoji
        ("\u{1F4BB}", "\"\\ud83d\\udcbb\""), // Laptop emoji
        ("build \u{1F680} ok", "\"build \\ud83d\\ude80 ok\""),
        ("\u{20000}", "\"\\ud840\\udc00\""), // CJK Extension B
    ];

    for (input, expected_json) in test_cases {
        let val = Value::String(input.to_string());
        let canonical = to_canonical_json_str(&val);
        assert_eq!(
            canonical, expected_json,
            "Input {:?} escaped canonical JSON mismatch: got {}, expected {}",
            input, canonical, expected_json
        );

        let mut direct_out = String::new();
        write_canonical_json_value(&val, &mut direct_out);
        assert_eq!(direct_out, expected_json);
    }
}

#[test]
fn test_adversarial_multiline_boot_script_oracle() {
    let complex_script = r#"#!/usr/bin/env bash
set -euo pipefail

# Unicode comment: 🚀 Rocket Launch — üñîçødè
echo "Step 1: Preparing build workspace..."
mkdir -p /mnt/runner-data/work

cat << 'EOF' > /mnt/runner-data/work/config.toml
[settings]
retries = 3
timeout_sec = 300
description = "Adversarial \"quoted\" test with \path\separators"
EOF

export PATH="/usr/local/bin:$PATH"
echo "All done \o/"
"#;

    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let mut manifest = fixture["vectors"][1]["input_manifest"].clone();
    manifest["host"]["boot_script"] = json!(complex_script);

    let p = serde_json::from_value::<RunnerPatch>(manifest.clone()).unwrap();
    let canonical = p.to_canonical_json();

    // Verify canonical json contains properly escaped newlines and unicode surrogates
    assert!(
        canonical.contains("\\ud83d\\ude80"),
        "Emoji must be escaped to surrogate pair"
    );
    assert!(
        canonical.contains("\\u00fc\\u00f1\\u00ee\\u00e7\\u00f8d\\u00e8"),
        "Accented chars must be escaped"
    );
    assert!(
        canonical.contains(r#"\"Adversarial"#),
        "Double quotes must be escaped with backslash"
    );
    assert!(
        canonical.contains(r#"\\path\\separators"#),
        "Backslashes must be double escaped"
    );

    let sha_raw = compute_canonical_patch_sha256(&manifest).unwrap();
    let sha_typed = p.patch_sha256();
    assert_eq!(
        sha_raw, sha_typed,
        "Raw and typed hash for multiline script must match"
    );
}

#[test]
fn test_golden_image_and_instance_template_deep_oracle() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let v6_manifest = fixture["vectors"][5]["input_manifest"].clone();
    let v6_sha = fixture["vectors"][5]["patch_sha256"].as_str().unwrap();

    // 1. Manifest with additive fields None / null matches legacy digest
    let mut m_null = v6_manifest.clone();
    m_null["source_image"] = Value::Null;
    m_null["golden_image_family"] = Value::Null;
    m_null["source_instance_template"] = Value::Null;
    let p_null = serde_json::from_value::<RunnerPatch>(m_null.clone()).unwrap();
    assert_eq!(p_null.patch_sha256(), v6_sha);
    assert_eq!(
        compute_canonical_patch_sha256(&m_null).unwrap(),
        fixture["vectors"][5]["raw_dict_sha256"].as_str().unwrap()
    );

    // 2. Setting each additive field produces deterministic, distinct hashes
    let mut m_img = v6_manifest.clone();
    m_img["source_image"] = json!("projects/homelab-424523/global/images/runner-golden-20261021");
    let p_img = serde_json::from_value::<RunnerPatch>(m_img).unwrap();
    let sha_img = p_img.patch_sha256();
    assert_ne!(sha_img, v6_sha);

    let mut m_fam = v6_manifest.clone();
    m_fam["golden_image_family"] =
        json!("projects/homelab-424523/global/imageFamilies/spot-runners-golden");
    let p_fam = serde_json::from_value::<RunnerPatch>(m_fam).unwrap();
    let sha_fam = p_fam.patch_sha256();
    assert_ne!(sha_fam, v6_sha);
    assert_ne!(sha_fam, sha_img);

    let mut m_tpl = v6_manifest.clone();
    m_tpl["source_instance_template"] =
        json!("projects/homelab-424523/global/instanceTemplates/runner-template-16vcpu");
    let p_tpl = serde_json::from_value::<RunnerPatch>(m_tpl).unwrap();
    let sha_tpl = p_tpl.patch_sha256();
    assert_ne!(sha_tpl, v6_sha);
    assert_ne!(sha_tpl, sha_img);
    assert_ne!(sha_tpl, sha_fam);

    // 3. Avalanche effect: a 1-character difference in source_image must change the SHA-256 completely
    let mut m_img_perturbed = v6_manifest.clone();
    m_img_perturbed["source_image"] =
        json!("projects/homelab-424523/global/images/runner-golden-20261022");
    let p_img_perturbed = serde_json::from_value::<RunnerPatch>(m_img_perturbed).unwrap();
    let sha_perturbed = p_img_perturbed.patch_sha256();
    assert_ne!(sha_perturbed, sha_img);
}

#[test]
fn test_sizing_floor_exhaustive_boundaries_oracle() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let base_valid = fixture["vectors"][1]["input_manifest"].clone();

    // Baseline is valid
    assert!(validate_patch_manifest(&base_valid).is_ok());

    // Boundary: min_vcpu (floor: 16)
    for vcpu in [0, 1, 8, 14, 15] {
        let mut m = base_valid.clone();
        m["hardware"]["min_vcpu"] = json!(vcpu);
        let errs = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("min_vcpu_below_floor")),
            "vcpu {} must be rejected",
            vcpu
        );
    }
    for vcpu in [16, 17, 32, 64] {
        let mut m = base_valid.clone();
        m["hardware"]["min_vcpu"] = json!(vcpu);
        assert!(
            validate_patch_manifest(&m).is_ok(),
            "vcpu {} must be accepted",
            vcpu
        );
    }

    // Boundary: min_ram_gib (floor: 32)
    for ram in [0, 1, 16, 30, 31] {
        let mut m = base_valid.clone();
        m["hardware"]["min_ram_gib"] = json!(ram);
        let errs = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("min_ram_gib_below_floor")),
            "ram {} must be rejected",
            ram
        );
    }
    for ram in [32, 33, 64, 128] {
        let mut m = base_valid.clone();
        m["hardware"]["min_ram_gib"] = json!(ram);
        assert!(
            validate_patch_manifest(&m).is_ok(),
            "ram {} must be accepted",
            ram
        );
    }

    // Boundary: runners_count (must be exactly 2)
    for cnt in [0, 1, 3, 4, 8] {
        let mut m = base_valid.clone();
        m["hardware"]["runners_count"] = json!(cnt);
        let errs = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("runners_count_must_be_2")),
            "runners_count {} must be rejected",
            cnt
        );
    }

    // Boundary: runner_vcpu (must be exactly 7)
    for vcpu in [0, 1, 6, 8, 14] {
        let mut m = base_valid.clone();
        m["hardware"]["runner_vcpu"] = json!(vcpu);
        let errs = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("runner_vcpu_must_be_7")),
            "runner_vcpu {} must be rejected",
            vcpu
        );
    }

    // Boundary: runner_ram_gib (must be exactly 14)
    for ram in [0, 1, 13, 15, 28] {
        let mut m = base_valid.clone();
        m["hardware"]["runner_ram_gib"] = json!(ram);
        let errs = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("runner_ram_gib_must_be_14")),
            "runner_ram_gib {} must be rejected",
            ram
        );
    }

    // Boundary: host_overhead_gib (must be exactly 4)
    for oh in [0, 1, 3, 5, 8] {
        let mut m = base_valid.clone();
        m["hardware"]["host_overhead_gib"] = json!(oh);
        let errs = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("host_overhead_gib_must_be_4")),
            "host_overhead_gib {} must be rejected",
            oh
        );
    }

    // Boundary: queued_jobs_threshold (floor: 20)
    for q in [0, 1, 10, 19] {
        let mut m = base_valid.clone();
        m["hardware"]["queued_jobs_threshold"] = json!(q);
        let errs = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("queued_jobs_threshold_below_floor")),
            "queued_jobs_threshold {} must be rejected",
            q
        );
    }
    for q in [20, 21, 50, 100] {
        let mut m = base_valid.clone();
        m["hardware"]["queued_jobs_threshold"] = json!(q);
        assert!(
            validate_patch_manifest(&m).is_ok(),
            "queued_jobs_threshold {} must be accepted",
            q
        );
    }
}

#[test]
fn test_permitted_machine_types_and_forbidden_mounts_oracle() {
    let fixture_str = include_str!("../../tests/fixtures/patch_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).unwrap();
    let base_valid = fixture["vectors"][1]["input_manifest"].clone();

    // Permitted machine types in extra_labels
    for mtype in PERMITTED_MACHINE_TYPES {
        let mut m = base_valid.clone();
        m["hardware"]["extra_labels"] = json!([format!("type-{}", mtype)]);
        assert!(
            validate_patch_manifest(&m).is_ok(),
            "Permitted type {} must pass",
            mtype
        );
    }

    // Unpermitted machine types
    let forbidden_types = [
        "c2-standard-16",
        "n2-standard-32",
        "e2-medium",
        "m1-megamem-96",
        "t2a-standard-16",
    ];
    for mtype in forbidden_types {
        let mut m = base_valid.clone();
        m["hardware"]["extra_labels"] = json!([format!("type-{}", mtype)]);
        let errs = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("unpermitted_machine_type")),
            "Forbidden machine type {} must be rejected",
            mtype
        );
    }

    // Forbidden mounts (exact and subpath)
    for forbidden in FORBIDDEN_MOUNTS {
        // Exact
        let mut m1 = base_valid.clone();
        m1["container"]["mounts"] = json!([{"source": forbidden, "target": "/mnt/test"}]);
        let errs1 = validate_patch_manifest(&m1).unwrap_err();
        assert!(
            errs1.iter().any(|e| e.contains("forbidden_mount_source")),
            "Exact forbidden mount {} must be rejected",
            forbidden
        );

        // Subpath
        let subpath = format!("{}/subfile", forbidden);
        let mut m2 = base_valid.clone();
        m2["container"]["mounts"] = json!([{"source": subpath, "target": "/mnt/test"}]);
        let errs2 = validate_patch_manifest(&m2).unwrap_err();
        assert!(
            errs2.iter().any(|e| e.contains("forbidden_mount_source")),
            "Subpath forbidden mount {} must be rejected",
            subpath
        );
    }

    // Allowed mount sources
    let allowed_mounts = [
        "/mnt/runner-data/cache",
        "/tmp/build-cache",
        "/data/scratch",
        "/var/log/custom",
    ];
    for allowed in allowed_mounts {
        let mut m = base_valid.clone();
        m["container"]["mounts"] = json!([{"source": allowed, "target": "/mnt/test"}]);
        assert!(
            validate_patch_manifest(&m).is_ok(),
            "Allowed mount {} must pass",
            allowed
        );
    }
}

#[test]
fn test_rollout_diff_classification_oracle() {
    let empty = RunnerPatch::default();

    // 1. empty vs empty => noop
    let diff = classify_patch_diff(&empty, &empty);
    assert_eq!(diff, "noop");

    // 2. host change (apt_packages) => hot_patch
    let mut host_changed = empty.clone();
    host_changed.host.apt_packages = vec!["htop".to_string()];
    let diff = classify_patch_diff(&empty, &host_changed);
    assert_eq!(diff, "hot_patch");

    // 3. container apt_packages addition => hot_patch
    let mut container_pkg_changed = empty.clone();
    container_pkg_changed.container.apt_packages = vec!["gcc".to_string()];
    let diff = classify_patch_diff(&empty, &container_pkg_changed);
    assert_eq!(diff, "hot_patch");

    // 4. container dockerfile_run change => recreate_containers
    let mut container_changed = empty.clone();
    container_changed.container.dockerfile_run = "cargo version".to_string();
    let diff = classify_patch_diff(&empty, &container_changed);
    assert_eq!(diff, "recreate_containers");

    // 5. container mounts change => recreate_containers
    let mut mount_changed = empty.clone();
    mount_changed.container.mounts = vec![spot_dashboard_edge::patch::PatchMountSpec {
        source: "/mnt/runner-data/cache".to_string(),
        target: "/work/cache".to_string(),
        read_only: false,
    }];
    let diff = classify_patch_diff(&empty, &mount_changed);
    assert_eq!(diff, "recreate_containers");

    // 6. hardware change => replace_vm
    let mut hw_changed = empty.clone();
    hw_changed.hardware.min_vcpu = Some(32);
    let diff = classify_patch_diff(&empty, &hw_changed);
    assert_eq!(diff, "replace_vm");

    // 5. golden image change in hardware => replace_vm
    let mut img_changed = empty.clone();
    img_changed.hardware.source_image = Some("projects/p/global/images/img-1".to_string());
    let diff = classify_patch_diff(&empty, &img_changed);
    assert_eq!(diff, "replace_vm");

    // 6. source instance template change in hardware => replace_vm
    let mut tpl_changed = empty.clone();
    tpl_changed.hardware.source_instance_template =
        Some("projects/p/global/instanceTemplates/t-1".to_string());
    let diff = classify_patch_diff(&empty, &tpl_changed);
    assert_eq!(diff, "replace_vm");

    // Check classify_rollout wrapper
    let rollout = classify_rollout(Some(&empty), &tpl_changed);
    assert_eq!(rollout.action, "replace_vm");
}

#[test]
fn test_multi_repo_merge_oracle() {
    let mut defaults = RunnerPatch::default();
    defaults.repository = "swack-tools".to_string();
    defaults.hardware.min_vcpu = Some(16);
    defaults.hardware.min_ram_gib = Some(32);
    defaults.host.apt_packages = vec!["git".to_string(), "curl".to_string()];

    let mut repo_override = RunnerPatch::default();
    repo_override.repository = "swack-tools/oxidex".to_string();
    repo_override.host.apt_packages = vec!["jq".to_string()];
    repo_override.hardware.extra_labels = vec!["type-c4d-highcpu-16".to_string()];

    let merged = merge_patches(&defaults, &repo_override);
    assert_eq!(merged.repository, "swack-tools/oxidex");
    assert_eq!(merged.hardware.min_vcpu, Some(16));
    assert_eq!(merged.hardware.min_ram_gib, Some(32));
    assert_eq!(
        merged.hardware.extra_labels,
        vec!["type-c4d-highcpu-16".to_string()]
    );
    // apt_packages must be unioned: git, curl, jq
    assert_eq!(
        merged.host.apt_packages,
        vec!["git".to_string(), "curl".to_string(), "jq".to_string()]
    );
}

#[test]
fn test_root_vs_hardware_golden_image_rollout_anomaly() {
    let empty = RunnerPatch::default();

    // Setting source_image at root:
    let mut root_img = empty.clone();
    root_img.source_image = Some("projects/p/global/images/img-1".to_string());

    // Root-level source_image currently classifies as "hot_patch" because classify_patch_diff
    // only inspects hardware, host.boot_script, host.sysctl, and container.
    let diff = classify_patch_diff(&empty, &root_img);
    assert_eq!(diff, "hot_patch");

    // Setting source_image inside hardware classifies as "replace_vm"
    let mut hw_img = empty.clone();
    hw_img.hardware.source_image = Some("projects/p/global/images/img-1".to_string());
    let diff_hw = classify_patch_diff(&empty, &hw_img);
    assert_eq!(diff_hw, "replace_vm");
}
