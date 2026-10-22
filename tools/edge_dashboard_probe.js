#!/usr/bin/env node
'use strict';

/**
 * @fileoverview Production health and observability probe for the serverless
 * Cloudflare Edge Worker dashboard (spot.swacktech.com).
 *
 * Probes the /api/health and /api/overview endpoints, validates JSON contracts,
 * measures round-trip response latencies, and emits human-readable or structured
 * JSON diagnostic telemetry.
 */

const { URL } = require('node:url');

const DEFAULT_BASE_URL = process.env.SPOT_DASHBOARD_URL || 'https://spot.swacktech.com';
const DEFAULT_TIMEOUT_MS = 10000;

/**
 * Parse command-line arguments into configuration options.
 * @param {string[]} argv Command-line arguments (process.argv.slice(2))
 * @returns {object} Parsed options object
 */
function parseArgs(argv) {
  const options = {
    baseUrl: DEFAULT_BASE_URL,
    timeoutMs: DEFAULT_TIMEOUT_MS,
    checkOverview: true,
    json: false,
    verbose: false,
    help: false,
  };

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') {
      options.help = true;
    } else if (arg === '--json') {
      options.json = true;
    } else if (arg === '--verbose' || arg === '-v') {
      options.verbose = true;
    } else if (arg === '--no-overview') {
      options.checkOverview = false;
    } else if (arg === '--overview') {
      options.checkOverview = true;
    } else if (arg === '--url' || arg === '-u') {
      if (i + 1 < argv.length) {
        options.baseUrl = argv[++i];
      } else {
        throw new Error('Flag --url requires a URL argument');
      }
    } else if (arg.startsWith('--url=')) {
      options.baseUrl = arg.slice('--url='.length);
    } else if (arg === '--timeout' || arg === '-t') {
      if (i + 1 < argv.length) {
        options.timeoutMs = parseInt(argv[++i], 10);
      } else {
        throw new Error('Flag --timeout requires a millisecond number');
      }
    } else if (arg.startsWith('--timeout=')) {
      options.timeoutMs = parseInt(arg.slice('--timeout='.length), 10);
    } else if (!arg.startsWith('-')) {
      // Positional argument interpreted as base URL
      options.baseUrl = arg;
    } else {
      throw new Error(`Unknown option: ${arg}`);
    }
  }

  if (Number.isNaN(options.timeoutMs) || options.timeoutMs <= 0) {
    throw new Error('Timeout must be a positive integer in milliseconds');
  }

  return options;
}

/**
 * Print CLI usage instructions.
 */
function printUsage() {
  console.log(`Usage: node tools/edge_dashboard_probe.js [options] [baseUrl]

Options:
  -u, --url <url>        Base URL of dashboard (default: ${DEFAULT_BASE_URL})
  -t, --timeout <ms>     Request timeout in milliseconds (default: ${DEFAULT_TIMEOUT_MS})
  --overview             Probe /api/overview in addition to /api/health (default: true)
  --no-overview          Probe only /api/health
  --json                 Output results as JSON
  -v, --verbose          Print verbose diagnostic details
  -h, --help             Show this help message

Exit codes:
  0 - All probed endpoints are healthy and contracts valid
  1 - Unhealthy response or failed assertion
  2 - Network, DNS, timeout, or configuration error
`);
}

/**
 * Perform a single GET probe against an endpoint.
 * @param {string} endpointUrl Complete URL to probe
 * @param {number} timeoutMs Timeout in milliseconds
 * @returns {Promise<object>} Probe outcome
 */
async function probeEndpoint(endpointUrl, timeoutMs) {
  const parsedUrl = new URL(endpointUrl);
  const startTime = Date.now();

  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);

    const response = await fetch(parsedUrl.toString(), {
      method: 'GET',
      headers: {
        'Accept': 'application/json',
        'User-Agent': 'spot-edge-probe/1.0',
      },
      signal: controller.signal,
    });

    clearTimeout(timer);
    const latencyMs = Date.now() - startTime;
    const contentType = response.headers.get('content-type') || '';
    const isJson = contentType.includes('application/json');

    let body = null;
    let text = '';
    if (isJson) {
      body = await response.json();
    } else {
      text = await response.text();
    }

    return {
      url: parsedUrl.toString(),
      status: response.status,
      statusText: response.statusText,
      contentType,
      latencyMs,
      ok: response.ok,
      data: body,
      rawText: text,
      error: null,
    };
  } catch (err) {
    const latencyMs = Date.now() - startTime;
    return {
      url: parsedUrl.toString(),
      status: 0,
      statusText: 'ClientError',
      contentType: '',
      latencyMs,
      ok: false,
      data: null,
      rawText: '',
      error: err.name === 'AbortError'
        ? `Request timed out after ${timeoutMs}ms`
        : err.message,
    };
  }
}

/**
 * Validate /api/health contract assertions.
 * @param {object} probeResult Result of /api/health probe
 * @returns {{valid: boolean, issues: string[]}}
 */
function validateHealthContract(probeResult) {
  const issues = [];
  if (!probeResult.ok) {
    issues.push(`HTTP status ${probeResult.status} ${probeResult.statusText}`);
  }
  if (!probeResult.contentType.includes('application/json')) {
    issues.push(`Expected application/json content-type, got '${probeResult.contentType}'`);
  }
  const data = probeResult.data;
  if (!data || typeof data !== 'object') {
    issues.push('Response body is not a valid JSON object');
    return { valid: false, issues };
  }

  if (data.status !== 'ok') {
    issues.push(`Expected status 'ok', received '${data.status}'`);
  }
  if (data.hydrated !== true) {
    issues.push(`Expected hydrated: true, received '${data.hydrated}'`);
  }
  if (data.reconciled !== true) {
    issues.push(`Expected reconciled: true, received '${data.reconciled}'`);
  }
  if (typeof data.state_version !== 'number' && typeof data.state_version !== 'undefined') {
    issues.push('state_version must be numeric when present');
  }

  return {
    valid: issues.length === 0,
    issues,
  };
}

/**
 * Validate /api/overview contract assertions.
 * @param {object} probeResult Result of /api/overview probe
 * @returns {{valid: boolean, issues: string[]}}
 */
function validateOverviewContract(probeResult) {
  const issues = [];
  if (!probeResult.ok) {
    issues.push(`HTTP status ${probeResult.status} ${probeResult.statusText}`);
  }
  if (!probeResult.contentType.includes('application/json')) {
    issues.push(`Expected application/json content-type, got '${probeResult.contentType}'`);
  }
  const data = probeResult.data;
  if (!data || typeof data !== 'object') {
    issues.push('Response body is not a valid JSON object');
    return { valid: false, issues };
  }

  return {
    valid: issues.length === 0,
    issues,
  };
}

/**
 * Execute edge probe test suite.
 * @param {object} options
 * @returns {Promise<{healthy: boolean, results: object}>}
 */
async function runProbes(options) {
  const base = options.baseUrl.replace(/\/+$/, '');
  const healthUrl = `${base}/api/health`;
  const overviewUrl = `${base}/api/overview`;

  const healthProbe = await probeEndpoint(healthUrl, options.timeoutMs);
  const healthValidation = validateHealthContract(healthProbe);

  let overviewProbe = null;
  let overviewValidation = null;
  if (options.checkOverview && !healthProbe.error) {
    overviewProbe = await probeEndpoint(overviewUrl, options.timeoutMs);
    overviewValidation = validateOverviewContract(overviewProbe);
  }

  const allOk = healthValidation.valid && (!overviewValidation || overviewValidation.valid);
  const hasNetworkError = Boolean(healthProbe.error || (overviewProbe && overviewProbe.error));

  return {
    healthy: allOk,
    hasNetworkError,
    timestamp: new Date().toISOString(),
    baseUrl: base,
    health: {
      probe: healthProbe,
      validation: healthValidation,
    },
    overview: overviewProbe ? {
      probe: overviewProbe,
      validation: overviewValidation,
    } : null,
  };
}

/**
 * Main command-line entrypoint.
 */
async function main() {
  let options;
  try {
    options = parseArgs(process.argv.slice(2));
  } catch (err) {
    console.error(`Error: ${err.message}\n`);
    printUsage();
    process.exit(2);
  }

  if (options.help) {
    printUsage();
    process.exit(0);
  }

  const report = await runProbes(options);

  if (options.json) {
    console.log(JSON.stringify(report, null, 2));
  } else {
    console.log(`Edge Dashboard Probe: ${report.baseUrl}`);
    console.log(`Timestamp: ${report.timestamp}\n`);

    // Health Report
    const h = report.health;
    const hStatus = h.validation.valid ? '✓ PASS' : '✗ FAIL';
    console.log(`[${hStatus}] /api/health (${h.probe.latencyMs}ms)`);
    if (h.probe.error) {
      console.log(`  Error: ${h.probe.error}`);
    } else {
      console.log(`  HTTP Status: ${h.probe.status}`);
      if (h.probe.data) {
        console.log(`  State Version: ${h.probe.data.state_version ?? 'n/a'}`);
        console.log(`  Writer Slot:   ${h.probe.data.writer_slot ?? 'n/a'}`);
        console.log(`  Cache Source:  ${h.probe.data.cache_source ?? 'n/a'}`);
        console.log(`  Reconciler:    ${h.probe.data.reconciler_ticks ?? 0} ticks`);
      }
      if (h.validation.issues.length > 0) {
        console.log('  Contract Issues:');
        for (const issue of h.validation.issues) {
          console.log(`    - ${issue}`);
        }
      }
    }

    // Overview Report
    if (report.overview) {
      const o = report.overview;
      const oStatus = o.validation.valid ? '✓ PASS' : '✗ FAIL';
      console.log(`\n[${oStatus}] /api/overview (${o.probe.latencyMs}ms)`);
      if (o.probe.error) {
        console.log(`  Error: ${o.probe.error}`);
      } else {
        console.log(`  HTTP Status: ${o.probe.status}`);
        if (o.validation.issues.length > 0) {
          console.log('  Contract Issues:');
          for (const issue of o.validation.issues) {
            console.log(`    - ${issue}`);
          }
        }
      }
    }

    console.log('');
    if (report.healthy) {
      console.log('Overall Status: HEALTHY');
    } else {
      console.log('Overall Status: UNHEALTHY');
    }
  }

  if (report.hasNetworkError) {
    process.exit(2);
  } else if (!report.healthy) {
    process.exit(1);
  } else {
    process.exit(0);
  }
}

if (require.main === module) {
  main().catch((err) => {
    console.error('Fatal probe error:', err);
    process.exit(2);
  });
}

module.exports = {
  parseArgs,
  probeEndpoint,
  validateHealthContract,
  validateOverviewContract,
  runProbes,
};
