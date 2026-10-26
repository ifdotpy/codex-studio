export const diagram = `flowchart LR
  subgraph N["Plain mode (account: false)"]
    direction LR
    U1["User account in the shared hub<br/>Simple cluster"] -->|"Cluster operator<br/>applies any manifest it receives"| P1["Provider cluster<br/>namespace taken from the kubeconfig file<br/>any resource type"]
    P1 --> H1["Hosting provider<br/>creates the cluster"]
  end

  subgraph A["Account mode (account: true) - used on staging"]
    direction LR
    U2["User account in the shared hub<br/>Simple cluster<br/>+ resource next to it"] -->|"sync-agent"| P2["Provider cluster<br/>copy of the resource"]
    P2 --> H2["Hosting provider<br/>creates the cluster"]
    C["Code check:<br/>only one resource kind<br/>from one API group"] -.-> U2
  end`;
export const html =
  '<style>.card{background:rgb(12, 85, 60);color:rgb(255, 255, 255)}</style><div class="card">Styled HTML preview</div>';
