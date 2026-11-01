// Synthetic fleet shown by scenes that need running hosts, which a local Worker can't produce.
const NOW = Math.floor(Date.now() / 1000);
const iso = t => new Date(t * 1000).toISOString().replace(/\.\d+Z$/, 'Z');
const HOUR = 3600;

function vm(o) {
  const hourly = o.hourly;
  const running = o.status === 'RUNNING';
  const started = NOW - Math.round(o.age * HOUR);
  const ended = running ? null : NOW - Math.round((o.endedAgo || 1) * HOUR);
  const runtime = ((ended || NOW) - started) / HOUR;
  return {
    id: o.id, instance_id: o.id, name: o.name, status: o.status, role: o.role, arch: o.arch,
    machine_type: o.machine, vcpu: o.vcpu, ram_gib: o.ram, cpu_platform: o.platform, benchmark_score: o.score,
    region: o.zone.replace(/-[a-z]$/, ''), zone: o.zone, display_location: o.location, latency_ms: 18,
    target: o.target, registration_target: o.target,
    data_disk_gb: o.disk, data_disk_type: o.diskType, disk_category: 'standard_ssd',
    runners_count: o.runners, runners_count_known: true, runner_vcpu: o.runnerVcpu, runner_ram_gib: o.runnerRam, runner_limits_known: true,
    workload_summary: o.role === 'cicd_runner' ? `${o.runners} GitHub Action Runner (${o.runnerVcpu} vCPU / ${o.runnerRam} GiB RAM each)` : `1 Dedicated Rust 1.99 Builder (${o.runnerVcpu} vCPU / ${o.runnerRam} GiB RAM)`,
    runtime_hours: +runtime.toFixed(2), hours_in_window: +Math.min(runtime, 24).toFixed(2),
    active_hourly_usd: running ? hourly : 0, active_daily_usd: running ? +(hourly * 24).toFixed(2) : 0, active_monthly_usd: running ? +(hourly * 730).toFixed(2) : 0,
    total_cost_in_window_usd: +(hourly * Math.min(runtime, 24)).toFixed(2),
    pricing_known: true,
    pricing: { compute_hourly_usd: hourly, compute_daily_usd: +(hourly * 24).toFixed(2), compute_monthly_usd: +(hourly * 730).toFixed(2), disk_hourly_usd: 0.0137, disk_daily_usd: 0.33, disk_monthly_usd: 9.86, tax_hourly_usd: 0, tax_daily_usd: 0, tax_monthly_usd: 0, tax_rate_percent: 0, total_hourly_usd: hourly, total_daily_usd: +(hourly * 24).toFixed(2), total_monthly_usd: +(hourly * 730).toFixed(2), pricing_source: 'gce_spot_list_price' },
    ssh_command: `gcloud compute ssh ${o.name} --zone=${o.zone}`,
    started_at: iso(started), started_epoch: started, deleted_at: ended ? iso(ended) : null, deleted_epoch: ended,
    deletion_trigger: o.trigger || null,
  };
}

const x86 = { arch: 'x86_64', machine: 'e2-standard-2', vcpu: 2, ram: 8, platform: 'Intel or AMD (E2)', score: 950, disk: 100, diskType: 'pd-balanced', role: 'cicd_runner', runners: 1, runnerVcpu: 2, runnerRam: 6, hourly: 0.0539 };
const arm = { arch: 'arm64', machine: 'n4a-standard-2', vcpu: 2, ram: 8, platform: 'Google Axion (Neoverse N3)', score: 1080, disk: 100, diskType: 'hyperdisk-balanced', role: 'cicd_runner', runners: 1, runnerVcpu: 2, runnerRam: 6, hourly: 0.0421 };
const builder = { arch: 'x86_64', machine: 'n4-highcpu-16', vcpu: 16, ram: 32, platform: 'Intel Emerald Rapids', score: 2620, disk: 375, diskType: 'hyperdisk-balanced', role: 'rust_builder', runners: 0, runnerVcpu: 16, runnerRam: 28, hourly: 0.3449 };
// GCP region facts: where each one is, its zones, machine families, CPUs, and disk types.
const REGIONS = [{"continent":"North America","country":"United States","cpu_platforms":["AMD Turin","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","hyperdisk-throughput","pd-ssd","pd-balanced","pd-standard"],"display_name":"Dallas, Texas, USA","enabled":true,"families":["c4d","n4d","n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":32.7767,"latency_ms":10,"lon":-96.797,"map_x":21.5,"map_y":38,"price_multiplier":"0.96","region":"us-south1","running_instances":[],"tax_rate_default":8.25,"zones":["us-south1-a","us-south1-b","us-south1-c"]},{"continent":"North America","country":"United States","cpu_platforms":["AMD Turin","Intel Granite Rapids","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","Intel Cascade Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","hyperdisk-throughput","pd-ssd","pd-balanced","pd-standard"],"display_name":"Council Bluffs, Iowa, USA","enabled":true,"families":["c4d","n4d","c4","n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":41.2619,"latency_ms":10,"lon":-95.8608,"map_x":22,"map_y":33,"price_multiplier":"0.92","region":"us-central1","running_instances":[],"tax_rate_default":7,"zones":["us-central1-a","us-central1-b","us-central1-c","us-central1-f"]},{"continent":"North America","country":"United States","cpu_platforms":["AMD Turin","Intel Granite Rapids","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","hyperdisk-throughput","pd-ssd","pd-balanced","pd-standard"],"display_name":"Moncks Corner, South Carolina, USA","enabled":true,"families":["c4d","n4d","c4","n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":33.196,"latency_ms":19,"lon":-80.0131,"map_x":26.5,"map_y":37.5,"price_multiplier":"0.94","region":"us-east1","running_instances":[],"tax_rate_default":7.5,"zones":["us-east1-b","us-east1-c","us-east1-d"]},{"continent":"North America","country":"United States","cpu_platforms":["Intel Granite Rapids","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","pd-ssd","pd-balanced","pd-standard"],"display_name":"Los Angeles, California, USA","enabled":true,"families":["c4","n4","n2d","n2","e2","c4a","n4a","t2a"],"lat":34.0522,"latency_ms":9,"lon":-118.2437,"map_x":15.5,"map_y":37,"price_multiplier":"1.08","region":"us-west2","running_instances":[],"tax_rate_default":9.5,"zones":["us-west2-a","us-west2-b","us-west2-c"]},{"continent":"North America","country":"Canada","cpu_platforms":["Intel Granite Rapids","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","pd-ssd","pd-balanced","pd-standard"],"display_name":"Montr\u00e9al, Qu\u00e9bec, Canada","enabled":true,"families":["c4","n4","n2d","n2","e2","c4a","n4a","t2a"],"lat":45.5017,"latency_ms":12,"lon":-73.5673,"map_x":28,"map_y":29.5,"price_multiplier":"0.99","region":"northamerica-northeast1","running_instances":[],"tax_rate_default":5,"zones":["northamerica-northeast1-a","northamerica-northeast1-b","northamerica-northeast1-c"]},{"continent":"South America","country":"Brazil","cpu_platforms":["Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","pd-ssd","pd-balanced","pd-standard"],"display_name":"S\u00e3o Paulo, Brazil","enabled":true,"families":["n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":-23.5505,"latency_ms":7,"lon":-46.6333,"map_x":35.5,"map_y":71,"price_multiplier":"1.18","region":"southamerica-east1","running_instances":[],"tax_rate_default":12,"zones":["southamerica-east1-a","southamerica-east1-b","southamerica-east1-c"]},{"continent":"Europe","country":"Belgium","cpu_platforms":["AMD Turin","Intel Granite Rapids","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","hyperdisk-throughput","pd-ssd","pd-balanced","pd-standard"],"display_name":"St. Ghislain, Belgium","enabled":true,"families":["c4d","n4d","c4","n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":50.4501,"latency_ms":8,"lon":3.8189,"map_x":49.5,"map_y":27.5,"price_multiplier":"0.98","region":"europe-west1","running_instances":[],"tax_rate_default":21,"zones":["europe-west1-b","europe-west1-c","europe-west1-d"]},{"continent":"Europe","country":"Netherlands","cpu_platforms":["AMD Turin","Intel Granite Rapids","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","hyperdisk-throughput","pd-ssd","pd-balanced","pd-standard"],"display_name":"Eemshaven, Netherlands","enabled":true,"families":["c4d","n4d","c4","n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":53.4386,"latency_ms":12,"lon":6.8355,"map_x":50.8,"map_y":25.5,"price_multiplier":"0.97","region":"europe-west4","running_instances":[],"tax_rate_default":21,"zones":["europe-west4-a","europe-west4-b","europe-west4-c"]},{"continent":"Europe","country":"Finland","cpu_platforms":["Intel Granite Rapids","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","pd-ssd","pd-balanced","pd-standard"],"display_name":"Hamina, Finland","enabled":true,"families":["c4","n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":60.5693,"latency_ms":11,"lon":27.1878,"map_x":56,"map_y":20.5,"price_multiplier":"0.95","region":"europe-north1","running_instances":[],"tax_rate_default":24,"zones":["europe-north1-a","europe-north1-b","europe-north1-c"]},{"continent":"Africa","country":"South Africa","cpu_platforms":["Intel Emerald Rapids","AMD Milan","Intel Ice Lake","Intel Cascade Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","pd-ssd","pd-balanced","pd-standard"],"display_name":"Johannesburg, South Africa","enabled":true,"families":["n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":-26.2041,"latency_ms":22,"lon":28.0473,"map_x":56.5,"map_y":73.5,"price_multiplier":"0.89","region":"africa-south1","running_instances":[],"tax_rate_default":15,"zones":["africa-south1-a","africa-south1-b","africa-south1-c"]},{"continent":"Middle East","country":"Israel","cpu_platforms":["Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","pd-ssd","pd-balanced","pd-standard"],"display_name":"Tel Aviv, Israel","enabled":true,"families":["n4","n2d","n2","e2","c4a","n4a","t2a"],"lat":32.0853,"latency_ms":9,"lon":34.7818,"map_x":58.8,"map_y":39.5,"price_multiplier":"1.04","region":"me-west1","running_instances":[],"tax_rate_default":17,"zones":["me-west1-a","me-west1-b","me-west1-c"]},{"continent":"Asia Pacific","country":"India","cpu_platforms":["Intel Granite Rapids","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","Intel Cascade Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","pd-ssd","pd-balanced","pd-standard"],"display_name":"Delhi, India","enabled":true,"families":["c4","n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":28.6139,"latency_ms":8,"lon":77.209,"map_x":70.5,"map_y":42,"price_multiplier":"0.84","region":"asia-south2","running_instances":[],"tax_rate_default":18,"zones":["asia-south2-a","asia-south2-b","asia-south2-c"]},{"continent":"Asia Pacific","country":"Hong Kong","cpu_platforms":["AMD Turin","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","hyperdisk-throughput","pd-ssd","pd-balanced","pd-standard"],"display_name":"Hong Kong, APAC","enabled":true,"families":["c4d","n4d","n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":22.3193,"latency_ms":12,"lon":114.1694,"map_x":79.8,"map_y":45.5,"price_multiplier":"0.91","region":"asia-east2","running_instances":[],"tax_rate_default":0,"zones":["asia-east2-a","asia-east2-b","asia-east2-c"]},{"continent":"Asia Pacific","country":"Singapore","cpu_platforms":["AMD Turin","Intel Emerald Rapids","AMD Milan","Intel Ice Lake","AMD Rome / E2 Dynamic","Google Axion (Neoverse V2)","Google Axion (Neoverse N3)","Ampere Altra (Neoverse N1)"],"disk_types":["hyperdisk-balanced","hyperdisk-throughput","pd-ssd","pd-balanced","pd-standard"],"display_name":"Jurong West, Singapore","enabled":true,"families":["c4d","n4","c2d","n2d","n2","e2","c4a","n4a","t2a"],"lat":1.3521,"latency_ms":12,"lon":103.8198,"map_x":77.2,"map_y":56.5,"price_multiplier":"0.93","region":"asia-southeast1","running_instances":[],"tax_rate_default":9,"zones":["asia-southeast1-a","asia-southeast1-b","asia-southeast1-c"]}];

const iowa = 'Council Bluffs, Iowa', sc = 'Moncks Corner, South Carolina', be = 'St. Ghislain, Belgium';

const INSTANCES = [
  vm({ ...x86, id: '100000000000001', name: 'runners-pool-a1b2c3-demo01', status: 'RUNNING', zone: 'us-central1-a', location: iowa, target: 'example-org', age: 2.4 }),
  vm({ ...x86, id: '100000000000002', name: 'runners-pool-a1b2c3-demo02', status: 'RUNNING', zone: 'us-central1-b', location: iowa, target: 'example-org', age: 0.8 }),
  vm({ ...arm, id: '100000000000003', name: 'runners-pool-d4e5f6-demo01', status: 'RUNNING', zone: 'us-east1-b', location: sc, target: 'example-org/service-a', age: 5.1 }),
  vm({ ...arm, id: '100000000000004', name: 'runners-pool-d4e5f6-demo02', status: 'RUNNING', zone: 'europe-west1-b', location: be, target: 'example-org/service-a', age: 1.2 }),
  vm({ ...builder, id: '100000000000005', name: 'builder-us-east1-b-demo01', status: 'RUNNING', zone: 'us-east1-b', location: sc, target: 'example-org', age: 7.5 }),
  vm({ ...x86, id: '100000000000006', name: 'runners-pool-a1b2c3-demo03', status: 'TERMINATED', zone: 'us-central1-c', location: iowa, target: 'example-org', age: 9, endedAgo: 6, trigger: 'idle_spin_down' }),
  vm({ ...arm, id: '100000000000007', name: 'runners-pool-d4e5f6-demo03', status: 'TERMINATED', zone: 'us-east1-c', location: sc, target: 'example-org/service-a', age: 14, endedAgo: 11, trigger: 'spot_preemption' }),
  vm({ ...builder, id: '100000000000008', name: 'builder-us-central1-a-demo02', status: 'TERMINATED', zone: 'us-central1-a', location: iowa, target: 'example-org', age: 20, endedAgo: 16, trigger: 'manual_delete' }),
];

// Rewrites an /api/overview response so the fleet looks alive, applying the page's own filters.
// disabled is an optional set of regions the scene has turned off.
function withFleet(j, req, disabled) {
  if (!j || !j.instances_view) return j;
  const q = req ? new URL(req.url()).searchParams : new URLSearchParams();
  const search = (q.get('search') || '').toLowerCase();
  const role = q.get('role') || 'all';
  const arch = q.get('arch') || 'all';
  const sortBy = q.get('sort_by') || 'total_cost_in_window_usd';
  const dir = q.get('sort_dir') === 'asc' ? 1 : -1;
  const includeDeleted = q.get('include_deleted') !== 'false';
  const windowHours = Number(q.get('window_hours') || 24);
  const hours = i => Math.min(i.runtime_hours, windowHours);
  const shown = INSTANCES
    .filter(i => includeDeleted || i.status === 'RUNNING')
    .filter(i => role === 'all' || i.role === role)
    .filter(i => arch === 'all' || i.arch === arch)
    .filter(i => !search || `${i.name} ${i.region} ${i.zone} ${i.machine_type} ${i.cpu_platform} ${i.deletion_trigger || ''}`.toLowerCase().includes(search))
    .map(i => ({ ...i, hours_in_window: +hours(i).toFixed(2), total_cost_in_window_usd: +(i.pricing.total_hourly_usd * hours(i)).toFixed(2) }))
    .sort((a, b) => (a[sortBy] > b[sortBy] ? dir : a[sortBy] < b[sortBy] ? -dir : 0));
  const running = INSTANCES.filter(i => i.status === 'RUNNING');
  const runners = running.filter(i => i.role === 'cicd_runner');
  const builders = running.filter(i => i.role === 'rust_builder');
  const sum = a => +a.reduce((t, i) => t + i.active_hourly_usd, 0).toFixed(4);
  const armRunners = runners.filter(i => i.arch === 'arm64');
  const burn = { active_hourly_burn_usd: sum(running), active_daily_burn_usd: +(sum(running) * 24).toFixed(2), active_monthly_burn_usd: +(sum(running) * 730).toFixed(2) };
  const counts = { running_runner_vms: runners.length, running_runner_slots: runners.length, running_builder_vms: builders.length };
  const inventory = { source: 'gce', stale: false, stale_reason: null, last_error: null, last_success_at: iso(NOW - 120), last_success_epoch: NOW - 120 };
  j.project = 'example-ci-fleet';
  j.fleet_inventory = inventory;
  j.fleet_summary = { ...j.fleet_summary, ...burn, ...counts, active_instances_count: running.length, running_builder_instances: builders.length, runner_hourly_burn_usd: sum(runners), builder_hourly_burn_usd: sum(builders), arm64_runner_vms: armRunners.length, arm64_runner_slots: armRunners.length, x86_64_runner_vms: runners.length - armRunners.length, x86_64_runner_slots: runners.length - armRunners.length, total_vcpus: running.reduce((t, i) => t + i.vcpu, 0), total_ram_gib: running.reduce((t, i) => t + i.ram, 0) };
  j.instances_view = { ...j.instances_view, ...burn, ...counts, fleet_inventory: inventory, instances: shown, running_count: running.length, deleted_count: INSTANCES.length - running.length, total_window_spend_usd: +shown.reduce((t, i) => t + i.total_cost_in_window_usd, 0).toFixed(2), window_cost_complete: true, window_hours: windowHours, sort_by: sortBy, sort_dir: dir > 0 ? 'asc' : 'desc', include_deleted: includeDeleted };
  j.pricing = { ...j.pricing, ...burn, ...counts, active_instances_count: running.length };
  j.regions = REGIONS.map(r => ({ ...r, enabled: !(disabled && disabled.has(r.region)), running_instances: running.filter(i => i.region === r.region) }));
  return j;
}

// A response for GET /api/observability.
function observability(url) {
  const q = new URL(url).searchParams;
  const role = q.get('role') || 'all';
  const query = (q.get('q') || '').toLowerCase();
  const severity = q.get('severity') || 'all';
  const pts = Array.from({ length: 36 }, (_, i) => {
    const wave = Math.sin(i / 4), wob = Math.cos(i / 2.3);
    return {
      timestamp: iso(NOW - (36 - i) * 100), epoch: NOW - (36 - i) * 100,
      cpu_utilization_pct: +(46 + 18 * wave + 5 * wob).toFixed(1), container_cpu_pct: +(38 + 16 * wave + 4 * wob).toFixed(1), target_cpu_pct: 80,
      mem_available_gib: +(9 - 2 * wave).toFixed(2), mem_cgroup_peak_gib: +(5.6 + 1.1 * wave).toFixed(2), mem_working_set_gib: +(3.9 + 0.8 * wave).toFixed(2),
      recommended_dense_runners: 6 + (i > 18 ? 2 : 0), active_runners: 4 + (i > 22 ? 2 : 0), active_workflows: 2 + Math.round(1.5 + 1.5 * wave),
      disk_read_mib_s: +(24 + 18 * wob).toFixed(1), disk_write_mib_s: +(36 + 22 * wave).toFixed(1), disk_utilization_pct: 41,
    };
  });
  const flow = (org, repo, wf, job, run, id) => ({ organization: org, repository: repo, workflow_name: wf, job_name: job, run_id: run, job_id: id, github_workflow_url: `https://github.com/${repo}/actions/runs/${run}/job/${id}` });
  const ciA = flow('example-org', 'example-org/service-a', 'CI', 'test (stable)', 9000000001, 2000000001);
  const ciB = flow('example-org', 'example-org/service-b', 'Release', 'package-linux', 9000000002, 2000000002);
  const rows = [
    ['INFO', 'gha_worker', 'runners-pool-a1b2c3-demo01', 'Running job: test (stable)', ciA],
    ['INFO', 'gha_worker', 'runners-pool-a1b2c3-demo01', 'cargo test --workspace: 214 passed, 0 failed', ciA],
    ['WARN', 'admission', 'runners-pool-a1b2c3-demo02', 'memory working set above 80% of the slot; holding the next lease', ciB],
    ['INFO', 'ops_agent', 'runners-pool-d4e5f6-demo01', 'cgroup v2 metrics exported (window 60s)', null],
    ['ERROR', 'kernel', 'runners-pool-d4e5f6-demo02', 'Out of memory: killed process 4312 (rustc) in container runner-2', ciB],
    ['INFO', 'gha_worker', 'builder-us-east1-b-demo01', 'cargo build --release finished in 214 s', null],
  ];
  const sevRank = { INFO: 0, WARN: 1, ERROR: 2 };
  const logs = rows
    .filter(r => severity === 'all' || severity === 'ALL' || sevRank[r[0]] >= sevRank[severity])
    .filter(r => !query || `${r[2]} ${r[3]} ${r[4] ? r[4].job_name : ''}`.toLowerCase().includes(query))
    .filter(r => role === 'all' || (role === 'rust_builder') === r[2].startsWith('builder'))
    .map((r, i) => ({ timestamp: iso(NOW - i * 90), epoch: NOW - i * 90, severity: r[0], source: r[1], instance_name: r[2], message: r[3], ...(r[4] || {}) }));
  const host = (name, zone, machine, roleName, repo, cpu, wf) => ({
    instance_name: name, zone, machine_type: machine, arch: machine.startsWith('n4a') ? 'arm64' : 'x86_64', vcpu: roleName === 'rust_builder' ? 16 : 2, ram_gib: roleName === 'rust_builder' ? 32 : 8,
    role: roleName, organization: 'example-org', repository: repo, cpu_utilization_pct: cpu, loadavg_1m: cpu / 40, mem_used_gib: 3.2, mem_utilization_pct: 40, observed_peak_working_set_gib: 3.9,
    baseline_runners: 1, current_runners: 2, recommended_runners: 3, extra_runners_unlocked: 2, active_workflows: wf, ops_agent_status: 'healthy', density_reason: 'CPU and RAM below the 80% targets',
  });
  const hosts = [
    host('runners-pool-a1b2c3-demo01', 'us-central1-a', 'e2-standard-2', 'cicd_runner', 'example-org/service-a', 44, [ciA]),
    host('runners-pool-d4e5f6-demo01', 'us-east1-b', 'n4a-standard-2', 'cicd_runner', 'example-org/service-b', 37, [ciB]),
    host('builder-us-east1-b-demo01', 'us-east1-b', 'n4-highcpu-16', 'rust_builder', '', 62, []),
  ].filter(h => role === 'all' || h.role === role);
  return {
    status: 'ok', generated_at: iso(NOW), window_hours: Number(q.get('window_hours') || 1),
    catalog: { roles: ['all', 'cicd_runner', 'rust_builder'], organizations: ['example-org', 'example-user'], repositories: ['example-org/service-a', 'example-org/service-b', 'example-user/app-one'], instances: hosts.map(h => h.instance_name), sources: ['all', 'gha_worker', 'ops_agent', 'admission', 'kernel'] },
    dynamic_density: { enabled: true, target_cpu_percent: 80, target_memory_percent: 80, max_runners_per_instance: 8, baseline_total_runners: 4, current_total_runners: 6, recommended_total_runners: 8, extra_runners_unlocked: 4, avg_peak_working_set_gib: 3.9, avg_cgroup_peak_gib: 5.6, avg_cpu_utilization_pct: 46, avg_mem_utilization_pct: 40, density_multiplier: 1.5 },
    instances: hosts, timeseries: pts, logs, total_matching_logs: logs.length,
  };
}

module.exports = { INSTANCES, REGIONS, withFleet, observability, NOW };
