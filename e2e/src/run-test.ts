#!/usr/bin/env npx ts-node
/**
 * Patroni HA Failover E2E Test
 *
 * This test deploys the postgres-ha template, verifies failover works correctly,
 * and cleans up afterwards. Exit code 0 = success, 1 = failure.
 */

import { config } from "dotenv";
import { RailwayAPI } from "./utils/railway-api";
import { DatabaseClient } from "./utils/database";
import { retry, sleep } from "./utils/retry";

config();

// Simple assertion helper
function assert(condition: boolean, message: string): asserts condition {
  if (!condition) {
    throw new Error(`Assertion failed: ${message}`);
  }
}

async function runFailoverTest(): Promise<void> {
  const token = process.env.E2E_TOKEN_PRODUCTION;
  if (!token) {
    throw new Error("E2E_TOKEN_PRODUCTION env var is not set");
  }

  const api = new RailwayAPI(token);
  let projectId: string | null = null;

  try {
    const TEST_DATA = `failover-test-${Date.now()}`;
    let environmentId: string;
    let databaseUrl: string;
    let testDataId: number;

    // =========================================
    // Step 1: Deploy postgres-ha template
    // =========================================
    console.log("Step 1: Deploying postgres-ha template...");
    const projectName = `e2e-postgres-ha-${Date.now()}`;
    const deployment = await api.deployTemplate("postgres-ha", projectName);
    projectId = deployment.projectId;
    console.log(`Template deployed, project ID: ${projectId}`);

    // =========================================
    // Step 2: Wait for project to be ready
    // =========================================
    console.log("Step 2: Waiting for services to be ready...");

    let project: any;
    let services: any[] = [];
    let haproxyService: any;
    let postgresServices: any[] = [];

    await retry(
      async () => {
        project = await api.getProject(projectId!);
        services = project.services.edges.map((e: any) => e.node);

        if (services.length < 7) {
          throw new Error(`Only ${services.length}/7 services found`);
        }

        haproxyService = services.find((s) => s.name.toLowerCase().includes("haproxy"));
        postgresServices = services.filter((s) => s.name.toLowerCase().includes("postgres"));

        if (!haproxyService) throw new Error("HAProxy service not found");
        if (postgresServices.length < 3) throw new Error(`Only ${postgresServices.length}/3 postgres services found`);
      },
      { maxAttempts: 30, delayMs: 10000 }
    );

    environmentId = project.environments.edges[0]?.node.id;
    console.log(`Found ${services.length} services, environment: ${environmentId}`);

    // Wait for HAProxy deployment
    console.log("Waiting for HAProxy deployment...");
    await api.waitForDeployment(haproxyService.id, environmentId, "SUCCESS", 300000);
    console.log("HAProxy is deployed");

    // Wait for postgres deployments
    console.log("Waiting for Postgres deployments...");
    for (const pg of postgresServices) {
      try {
        await api.waitForDeployment(pg.id, environmentId, "SUCCESS", 300000);
        console.log(`${pg.name} is deployed`);
      } catch (e) {
        console.log(`${pg.name} deployment check: ${e}`);
      }
    }

    // =========================================
    // Step 3: Get/Create TCP Proxy for HAProxy
    // =========================================
    console.log("Step 3: Setting up public networking...");

    let tcpProxies = await api.getTcpProxies(haproxyService.id, environmentId);
    let tcpProxy = tcpProxies.find((p) => p.applicationPort === 5432);

    if (!tcpProxy) {
      console.log("Creating TCP proxy on port 5432...");
      tcpProxy = await api.createTcpProxy(haproxyService.id, environmentId, 5432);
    }

    console.log(`TCP Proxy: ${tcpProxy.domain}:${tcpProxy.proxyPort}`);

    // Wait for DNS propagation
    console.log("Waiting for DNS propagation...");
    await sleep(30000);

    // =========================================
    // Step 4: Get database credentials
    // =========================================
    console.log("Step 4: Getting database credentials...");

    const postgresService = postgresServices[0];
    const variables = await api.getVariables(projectId, environmentId, postgresService.id);

    const pgUser = variables.POSTGRES_USER || "railway";
    const pgPassword = variables.POSTGRES_PASSWORD;
    const pgDatabase = variables.POSTGRES_DB || "railway";

    if (!pgPassword) {
      throw new Error("POSTGRES_PASSWORD not found in variables");
    }

    databaseUrl = `postgresql://${pgUser}:${pgPassword}@${tcpProxy.domain}:${tcpProxy.proxyPort}/${pgDatabase}`;
    console.log(`Database URL: postgresql://${pgUser}:***@${tcpProxy.domain}:${tcpProxy.proxyPort}/${pgDatabase}`);

    // =========================================
    // Step 5: Connect and write test data
    // =========================================
    console.log("Step 5: Connecting to database and writing test data...");
    const db = new DatabaseClient(databaseUrl);

    await retry(
      async () => {
        await db.connect(1, 0);
      },
      { maxAttempts: 30, delayMs: 10000 }
    );

    try {
      await db.createTestTable();
      testDataId = await db.insertTestData(TEST_DATA);
      console.log(`Test data written with ID: ${testDataId}`);

      const verified = await db.verifyTestData(testDataId, TEST_DATA);
      assert(verified, "Test data verification failed");
      console.log("Test data verified");
    } finally {
      await db.disconnect();
    }

    // =========================================
    // Step 6: Identify and remove primary
    // =========================================
    console.log("Step 6: Identifying and removing primary...");

    let primaryService: any = null;

    for (const pg of postgresServices) {
      let patroniProxies = await api.getTcpProxies(pg.id, environmentId);
      let patroniProxy = patroniProxies.find((p) => p.applicationPort === 8008);

      if (!patroniProxy) {
        console.log(`Creating Patroni TCP proxy for ${pg.name}...`);
        patroniProxy = await api.createTcpProxy(pg.id, environmentId, 8008);
        await sleep(5000);
      }

      try {
        const response = await fetch(
          `http://${patroniProxy.domain}:${patroniProxy.proxyPort}/cluster`,
          { signal: AbortSignal.timeout(10000) }
        );
        const cluster = await response.json();
        console.log(`${pg.name} cluster info:`, JSON.stringify(cluster.members?.map((m: any) => ({ name: m.name, role: m.role })) || []));

        const leader = cluster.members?.find((m: any) => m.role === "leader");
        if (leader) {
          const leaderService = postgresServices.find(
            (s: any) => leader.name === s.name || leader.name === s.name.replace(/-/g, "")
          );
          if (leaderService) {
            primaryService = leaderService;
            console.log(`Found leader: ${leaderService.name} (from ${pg.name}'s cluster view)`);
            break;
          }
        }
      } catch (error: any) {
        console.log(`Failed to query Patroni on ${pg.name}: ${error.message}`);
      }
    }

    if (!primaryService) {
      throw new Error("Could not identify primary node");
    }

    console.log(`Primary identified as: ${primaryService.name}`);

    const primaryDeployments = await api.getServiceDeployments(primaryService.id, environmentId);
    const primaryDeployment = primaryDeployments.find((d) => d.status === "SUCCESS");

    if (!primaryDeployment) {
      throw new Error("No active deployment found for primary");
    }

    // Start long-running query before killing primary
    console.log("Starting long-running query on primary...");
    const longQueryDb = new DatabaseClient(databaseUrl);
    await longQueryDb.connect(1, 0);

    let queryFailed = false;
    let queryError: string | null = null;
    longQueryDb.query("SELECT pg_sleep(120)")
      .then(() => { queryFailed = false; })
      .catch((err) => { queryFailed = true; queryError = err.message; });

    await sleep(2000);

    // Remove the primary
    console.log(`Removing deployment: ${primaryDeployment.id}`);
    const removePromise = api.removeDeployment(primaryDeployment.id)
      .then(() => console.log("Primary deployment removed"))
      .catch((err) => console.log(`Remove deployment error (may be expected): ${err.message}`));

    // Wait for long-running query to fail
    console.log("Waiting for long-running query to fail...");

    const startWait = Date.now();
    while (Date.now() - startWait < 90000) {
      if (queryFailed) {
        console.log(`Long-running query failed as expected: ${queryError}`);
        break;
      }
      await sleep(1000);
    }

    await removePromise;

    if (!queryFailed) {
      throw new Error("Long-running query did not fail within 90s - primary may not have died!");
    }

    try {
      await longQueryDb.disconnect();
    } catch {
      // Connection already dead
    }

    // =========================================
    // Step 7: Verify failover via Patroni
    // =========================================
    console.log("Step 7: Verifying failover via Patroni...");

    const remainingNode = postgresServices.find((pg: any) => pg.id !== primaryService.id);
    const remainingProxies = await api.getTcpProxies(remainingNode.id, environmentId);
    let patroniProxy = remainingProxies.find((p: any) => p.applicationPort === 8008);
    if (!patroniProxy) {
      patroniProxy = await api.createTcpProxy(remainingNode.id, environmentId, 8008);
      await sleep(5000);
    }

    let sawNewLeader = false;
    let newLeaderName: string | null = null;
    const maxWaitMs = 120000;
    const startTime = Date.now();
    const removedNodeName = primaryService.name.replace(/-/g, "");

    while (Date.now() - startTime < maxWaitMs) {
      try {
        const response = await fetch(
          `http://${patroniProxy.domain}:${patroniProxy.proxyPort}/cluster`,
          { signal: AbortSignal.timeout(5000) }
        );
        const cluster = await response.json();
        const leader = cluster.members?.find((m: any) => m.role === "leader");

        if (leader && leader.name !== removedNodeName) {
          sawNewLeader = true;
          newLeaderName = leader.name;
          console.log(`Patroni shows new leader: ${leader.name} (removed: ${removedNodeName})`);
          break;
        } else if (leader) {
          console.log(`Patroni still shows old leader: ${leader.name}`);
        } else {
          console.log("Patroni shows no leader - election in progress");
        }
      } catch (e: any) {
        console.log(`Patroni check failed: ${e.message}`);
      }

      await sleep(3000);
    }

    if (!sawNewLeader) {
      throw new Error("Failover did not occur - Patroni still shows old leader or no leader");
    }

    console.log(`Confirmed: Failover complete. New leader: ${newLeaderName}`);

    // =========================================
    // Step 8: Reconnect to new primary
    // =========================================
    console.log("Step 8: Reconnecting to new primary...");

    const failoverDb = new DatabaseClient(databaseUrl);

    await retry(
      async () => {
        await failoverDb.connect(1, 0);
        const isReadOnly = await failoverDb.isReadOnly();
        if (isReadOnly) {
          await failoverDb.disconnect();
          throw new Error("Connected to replica, waiting for new primary");
        }
      },
      { maxAttempts: 30, delayMs: 3000 }
    );

    console.log("Successfully reconnected to new primary after failover");

    // =========================================
    // Step 9: Verify data integrity
    // =========================================
    console.log("Step 9: Verifying data integrity...");

    try {
      const dataVerified = await failoverDb.verifyTestData(testDataId, TEST_DATA);
      assert(dataVerified, "Data integrity check failed - test data not found after failover");
      console.log("Data integrity verified");

      const newDataId = await failoverDb.insertTestData(`post-failover-${Date.now()}`);
      assert(newDataId > 0, "Post-failover write failed");
      console.log(`Post-failover write successful (ID: ${newDataId})`);
    } finally {
      await failoverDb.disconnect();
    }

    console.log("===========================================");
    console.log("FAILOVER TEST COMPLETED SUCCESSFULLY");
    console.log("===========================================");

  } finally {
    // Always cleanup
    if (projectId) {
      console.log(`Cleaning up project: ${projectId}`);
      try {
        await api.deleteProject(projectId);
        console.log("Project deleted successfully");
      } catch (error) {
        console.log(`Cleanup failed: ${error}`);
      }
    }
  }
}

// Run the test
runFailoverTest()
  .then(() => {
    console.log("\nTest passed!");
    process.exit(0);
  })
  .catch((error) => {
    console.error("\nTest failed:", error.message);
    process.exit(1);
  });
