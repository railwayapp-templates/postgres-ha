const BACKBOARD_URL = "https://backboard.railway.com";

export class RailwayAPI {
  constructor(private token: string) {}

  private async graphql<T>(query: string, variables?: Record<string, any>): Promise<T> {
    const response = await fetch(`${BACKBOARD_URL}/graphql/v2`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${this.token}`,
      },
      body: JSON.stringify({ query, variables }),
    });

    const text = await response.text();
    let result;
    try {
      result = JSON.parse(text);
    } catch {
      throw new Error(`API returned non-JSON (status ${response.status}): ${text.slice(0, 200)}`);
    }

    if (result.errors) {
      throw new Error(`GraphQL error: ${JSON.stringify(result.errors)}`);
    }
    return result.data;
  }

  async getTemplate(code: string): Promise<{ id: string; serializedConfig: any }> {
    const data = await this.graphql<any>(
      `
      query template($code: String!) {
        template(code: $code) {
          id
          serializedConfig
        }
      }
      `,
      { code }
    );
    return data.template;
  }

  async deployTemplate(templateCode: string, projectName?: string): Promise<{ projectId: string; workflowId: string }> {
    // First get the template info
    const template = await this.getTemplate(templateCode);

    const input: any = {
      templateId: template.id,
      serializedConfig: template.serializedConfig,
    };

    // Add project name if provided
    if (projectName) {
      input.projectName = projectName;
    }

    const data = await this.graphql<any>(
      `
      mutation templateDeployV2($input: TemplateDeployV2Input!) {
        templateDeployV2(input: $input) {
          projectId
          workflowId
        }
      }
      `,
      { input }
    );
    return data.templateDeployV2;
  }

  async getProject(projectId: string): Promise<any> {
    const data = await this.graphql<any>(
      `
      query project($id: String!) {
        project(id: $id) {
          id
          name
          services {
            edges {
              node {
                id
                name
              }
            }
          }
          environments {
            edges {
              node {
                id
                name
              }
            }
          }
        }
      }
      `,
      { id: projectId }
    );
    return data.project;
  }

  async getService(serviceId: string): Promise<any> {
    const data = await this.graphql<any>(
      `
      query service($id: String!) {
        service(id: $id) {
          id
          name
          deployments {
            edges {
              node {
                id
                status
              }
            }
          }
        }
      }
      `,
      { id: serviceId }
    );
    return data.service;
  }

  async getServiceDeployments(serviceId: string, environmentId: string): Promise<any[]> {
    const data = await this.graphql<any>(
      `
      query deployments($serviceId: String!, $environmentId: String!) {
        deployments(
          first: 10
          input: { serviceId: $serviceId, environmentId: $environmentId }
        ) {
          edges {
            node {
              id
              status
              staticUrl
            }
          }
        }
      }
      `,
      { serviceId, environmentId }
    );
    return data.deployments.edges.map((e: any) => e.node);
  }

  async getVariables(projectId: string, environmentId: string, serviceId: string): Promise<Record<string, string>> {
    const data = await this.graphql<any>(
      `
      query variables($projectId: String!, $environmentId: String!, $serviceId: String!) {
        variables(projectId: $projectId, environmentId: $environmentId, serviceId: $serviceId)
      }
      `,
      { projectId, environmentId, serviceId }
    );
    return data.variables;
  }

  async getTcpProxies(serviceId: string, environmentId: string): Promise<any[]> {
    const data = await this.graphql<any>(
      `
      query tcpProxies($serviceId: String!, $environmentId: String!) {
        tcpProxies(serviceId: $serviceId, environmentId: $environmentId) {
          id
          domain
          proxyPort
          applicationPort
        }
      }
      `,
      { serviceId, environmentId }
    );
    return data.tcpProxies || [];
  }

  async createTcpProxy(serviceId: string, environmentId: string, applicationPort: number): Promise<any> {
    const data = await this.graphql<any>(
      `
      mutation tcpProxyCreate($input: TCPProxyCreateInput!) {
        tcpProxyCreate(input: $input) {
          id
          domain
          proxyPort
          applicationPort
        }
      }
      `,
      {
        input: {
          serviceId,
          environmentId,
          applicationPort,
        },
      }
    );
    return data.tcpProxyCreate;
  }

  async restartDeployment(deploymentId: string): Promise<boolean> {
    const data = await this.graphql<any>(
      `
      mutation deploymentRestart($id: String!) {
        deploymentRestart(id: $id)
      }
      `,
      { id: deploymentId }
    );
    return data.deploymentRestart;
  }

  async removeDeployment(deploymentId: string, retries: number = 3): Promise<boolean> {
    for (let i = 0; i < retries; i++) {
      try {
        const data = await this.graphql<any>(
          `
          mutation deploymentRemove($id: String!) {
            deploymentRemove(id: $id)
          }
          `,
          { id: deploymentId }
        );
        return data.deploymentRemove;
      } catch (error: any) {
        console.log(`removeDeployment attempt ${i + 1}/${retries} failed: ${error.message}`);
        if (i === retries - 1) throw error;
        await new Promise(r => setTimeout(r, 5000));
      }
    }
    return false;
  }

  async deleteProject(projectId: string): Promise<boolean> {
    const data = await this.graphql<any>(
      `
      mutation projectDelete($id: String!) {
        projectDelete(id: $id)
      }
      `,
      { id: projectId }
    );
    return data.projectDelete;
  }

  async waitForDeployment(
    serviceId: string,
    environmentId: string,
    status: string = "SUCCESS",
    timeoutMs: number = 300000
  ): Promise<any> {
    const startTime = Date.now();
    while (Date.now() - startTime < timeoutMs) {
      const deployments = await this.getServiceDeployments(serviceId, environmentId);
      const deployment = deployments[0];

      if (deployment?.status === status) {
        return deployment;
      }

      if (deployment?.status === "FAILED" || deployment?.status === "CRASHED") {
        throw new Error(`Deployment failed with status: ${deployment.status}`);
      }

      console.log(`Waiting for deployment... (current: ${deployment?.status || "none"})`);
      await new Promise((r) => setTimeout(r, 10000));
    }
    throw new Error(`Deployment did not reach ${status} within ${timeoutMs}ms`);
  }
}
