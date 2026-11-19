import {
  Activity,
  Boxes,
  Database,
  GitBranch,
  KeyRound,
  Network,
  ShieldCheck,
  Zap,
  type LucideIcon,
} from "lucide-react";

export const GITHUB_REPO_URL = "https://github.com/ded-lmfao/WebDep";
export const GITHUB_RELEASES_URL = `${GITHUB_REPO_URL}/releases/latest`;

const INSTALLER = "curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash -s --";

export const HERO_INSTALL = `${INSTALLER} both`;

export interface Feature {
  icon: LucideIcon;
  title: string;
  text: string;
}

export const FEATURES: Feature[] = [
  {
    icon: GitBranch,
    title: "Git deploys",
    text: "Push-to-deploy from GitHub with a live 7-stage pipeline, releases and one-click rollback.",
  },
  {
    icon: Boxes,
    title: "Websites & services",
    text: "Node, Python, Go, Rust, Java, PHP and static sites, plus 24/7 workers and bots.",
  },
  {
    icon: Database,
    title: "Managed PostgreSQL",
    text: "Isolated native clusters per app with SCRAM auth and one-click DATABASE_URL linking.",
  },
  {
    icon: ShieldCheck,
    title: "Automatic HTTPS",
    text: "Caddy issues and renews certificates for every domain you attach.",
  },
  {
    icon: KeyRound,
    title: "Secure by default",
    text: "Passkeys, TOTP 2FA, per-user access levels and an AES-256-GCM encrypted .env vault.",
  },
  {
    icon: Activity,
    title: "Live telemetry",
    text: "CPU, memory, disk and network per app and per server, streamed in real time.",
  },
];

export interface NodeFact {
  icon: LucideIcon;
  title: string;
  text: string;
}

export const NODE_FACTS: NodeFact[] = [
  {
    icon: Network,
    title: "One dashboard, many servers",
    text: "Add a VPS with one command from Nodes → Add Node. It enrolls with the dashboard and appears in the fleet within seconds.",
  },
  {
    icon: Network,
    title: "Full WireGuard mesh",
    text: "Every server is peered with every other server on a private 10.200.x.x network, so apps on one node reach databases on another securely.",
  },
  {
    icon: Zap,
    title: "Move apps between nodes",
    text: "Shift a running app to another VPS with a direct node-to-node transfer, health check and DNS cutover card.",
  },
  {
    icon: Boxes,
    title: "Per-node runtimes",
    text: "See which runtimes each server has and install Node.js, Python, Java, Go and more in one click.",
  },
];

export interface DemoView {
  id: string;
  label: string;
}

/** Panel pages the inline live demo can switch to (ids match the panel's routes). */
export const DEMO_VIEWS: DemoView[] = [
  { id: "overview", label: "Overview" },
  { id: "projects", label: "Websites" },
  { id: "services", label: "Services" },
  { id: "databases", label: "Databases" },
  { id: "nodes", label: "Nodes" },
  { id: "modules", label: "Modules" },
  { id: "settings", label: "Settings" },
];

export type InstallOs = "linux" | "windows";

export interface InstallRole {
  id: string;
  label: string;
  hint: string;
  command: string;
}

export interface InstallPlatform {
  label: string;
  shell: string;
  roles: InstallRole[];
  /** Shown under the command: where to open the panel and find the first login. */
  after: { url: string; login: string };
}

const PS_INSTALLER = "& ([scriptblock]::Create((irm https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.ps1)))";

export const INSTALL_PLATFORMS: Record<InstallOs, InstallPlatform> = {
  linux: {
    label: "Linux",
    shell: "Ubuntu, Debian, RHEL · x86_64 & arm64 · run as root or with sudo",
    roles: [
      {
        id: "both",
        label: "Core + Node",
        hint: "Dashboard and your apps on this server. Best for one VPS.",
        command: `${INSTALLER} both`,
      },
      {
        id: "core",
        label: "Core only",
        hint: "Dashboard only, hosts no apps. For larger fleets.",
        command: `${INSTALLER} core`,
      },
      {
        id: "node",
        label: "Node only",
        hint: "Joins an existing dashboard. Pick 3, then paste the URL and token from Nodes → Add Node.",
        command: "curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash",
      },
    ],
    after: { url: "http://YOUR_SERVER_IP:4600", login: "/opt/webdep/.env" },
  },
  windows: {
    label: "Windows",
    shell: "PowerShell · no admin needed · starts automatically at logon",
    roles: [
      {
        id: "both",
        label: "Core + Node",
        hint: "Dashboard and your apps on this PC or Windows server.",
        command: `${PS_INSTALLER} both`,
      },
      {
        id: "core",
        label: "Core only",
        hint: "Dashboard only, hosts no apps.",
        command: `${PS_INSTALLER} core`,
      },
      {
        id: "node",
        label: "Node only",
        hint: "Joins an existing dashboard. Pick 3, then paste the URL and token from Nodes → Add Node.",
        command: "irm https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.ps1 | iex",
      },
    ],
    after: { url: "http://localhost:4600", login: "%LOCALAPPDATA%\\WebDep\\.env" },
  },
};

export const STATS = [
  { value: "~20 MB", label: "RAM for the control plane" },
  { value: "1", label: "static binary, no Docker" },
  { value: "x86_64 · arm64", label: "Linux and Windows" },
];
