import { useState } from "react";
import { AnimatePresence, m } from "motion/react";
import { ArrowUpRight } from "lucide-react";
import { CommandLine } from "@/components/CopyButton";
import { LinuxIcon, WindowsIcon } from "@/components/OsIcons";
import Reveal, { SectionHeading } from "@/components/Reveal";
import { GITHUB_REPO_URL, INSTALL_PLATFORMS, detectInstallOs, type InstallOs } from "@/data/site";

const OS_LIST: InstallOs[] = ["linux", "windows"];

export default function Install() {
  const [os, setOs] = useState<InstallOs>(detectInstallOs);
  const [roleId, setRoleId] = useState("both");
  const platform = INSTALL_PLATFORMS[os];
  const role = platform.roles.find((r) => r.id === roleId) ?? platform.roles[0];

  return (
    <section id="install" className="scroll-mt-20 border-t border-border px-4 py-24 sm:px-6">
      <SectionHeading
        eyebrow="Install"
        title="Up and running in a minute"
        text="Paste one line. It sets up the service, firewall and your admin login."
      />

      <Reveal className="mx-auto mt-12 max-w-2xl">
        {/* Operating system */}
        <div className="mx-auto flex w-fit gap-1 rounded-full border border-border bg-foreground/[0.03] p-1">
          {OS_LIST.map((id) => (
            <button
              key={id}
              type="button"
              onClick={() => setOs(id)}
              aria-label={INSTALL_PLATFORMS[id].label}
              aria-pressed={os === id}
              title={INSTALL_PLATFORMS[id].label}
              className={`relative grid h-10 w-16 place-items-center rounded-full transition-colors ${
                os === id ? "text-background" : "text-muted-foreground hover:text-foreground"
              }`}
            >
              {os === id ? (
                <m.span layoutId="install-os" className="absolute inset-0 rounded-full bg-foreground" transition={{ type: "spring", stiffness: 420, damping: 34 }} />
              ) : null}
              <span className="relative">{id === "linux" ? <LinuxIcon /> : <WindowsIcon className="size-[18px]" />}</span>
            </button>
          ))}
        </div>
        <p className="mt-3 text-center text-xs text-muted-foreground">
          <span className="font-medium text-foreground">{platform.label}</span> · {platform.shell}
        </p>

        {/* What to install */}
        <div className="mt-8 grid gap-2 sm:grid-cols-3">
          {platform.roles.map((r) => (
            <button
              key={r.id}
              type="button"
              onClick={() => setRoleId(r.id)}
              className={`rounded-xl border px-4 py-3 text-left transition-colors ${
                role.id === r.id ? "border-accent/60 bg-accent/[0.07]" : "border-border hover:bg-foreground/[0.03]"
              }`}
            >
              <span className="block text-sm font-medium text-foreground">{r.label}</span>
              <span className="mt-0.5 block text-xs leading-snug text-muted-foreground">{r.hint}</span>
            </button>
          ))}
        </div>

        <div className="mt-4 min-h-[56px]">
          <AnimatePresence mode="wait" initial={false}>
            <m.div
              key={`${os}-${role.id}`}
              initial={{ opacity: 0, y: 8 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, y: -8 }}
              transition={{ duration: 0.2 }}
            >
              <CommandLine command={role.command} prompt={os === "windows" ? "PS>" : "$"} />
            </m.div>
          </AnimatePresence>
        </div>

        <p className="mt-6 text-sm leading-relaxed text-muted-foreground">
          {role.id === "node" ? (
            <>
              The server appears under <span className="text-foreground">Nodes</span> in your dashboard once it joins. Tip: Nodes → Add
              Node also gives you a single line with the URL and token already filled in.{" "}
            </>
          ) : (
            <>
              Then open <code className="font-mono text-foreground">{platform.after.url}</code>. Your first login is in{" "}
              <code className="font-mono text-foreground">{platform.after.login}</code>. Run the same line again to update.{" "}
            </>
          )}
          <a href={`${GITHUB_REPO_URL}#readme`} target="_blank" rel="noreferrer" className="inline-flex items-center gap-0.5 text-foreground underline-offset-4 hover:underline">
            README on GitHub <ArrowUpRight className="size-3.5" />
          </a>
        </p>
      </Reveal>
    </section>
  );
}
