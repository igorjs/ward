# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.x (latest) | Yes |

Only the latest version receives security patches. Upgrade to the latest version before reporting.

## Reporting a Vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

To report a vulnerability, email: **oss@mail.igorjs.io**

Include:
- Description of the vulnerability
- Steps to reproduce
- Affected versions
- Impact assessment (what can an attacker do?)
- Suggested fix (if you have one)

### What to expect

- **Acknowledgement** within 48 hours
- **Assessment** within 7 days (severity, affected scope, fix plan)
- **Fix and disclosure** within 30 days for critical issues, 90 days for others

If the report is accepted, you will be credited in the release notes (unless you prefer anonymity).

If the report is declined (not a vulnerability, or out of scope), you will receive an explanation and may open a public issue.

### Scope

The following are in scope:
- Sandbox escape (code execution outside the sandbox boundary)
- Network isolation bypass (traffic escaping egress rules)
- Container escape or privilege escalation
- Resource limit bypass (CPU, memory, PID limits)
- Denial of service via crafted API requests
- Authentication/authorisation bypass on the daemon API

The following are out of scope:
- Vulnerabilities in containerd or gVisor themselves (report upstream)
- Issues requiring root access to the host (ward requires root to operate)
- Social engineering attacks
- Issues in test files or development tooling

## Security Design Principles

- **Defence in depth**: gVisor provides userspace kernel isolation on top of container namespaces
- **Principle of least privilege**: sandboxes get only the resources and network access explicitly granted
- **Egress filtering by default**: all outbound traffic is denied unless explicitly allowed
- **No eval, no shell injection**: all container commands are passed as structured arrays, never shell strings
