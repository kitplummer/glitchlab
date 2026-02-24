# Seams: Engineering General Patterns

This document outlines general engineering patterns within the Seams project.

## Engineering-Specific Patterns

### `crates/eng-org`

The `crates/eng-org` component provides foundational organizational structures and utilities specifically designed for engineering tasks within the Seams project. It aims to standardize common engineering patterns and facilitate consistent development practices across various modules.

### `glitchlab/agents`

The `glitchlab/agents` directory houses a collection of specialized agents, each responsible for a distinct phase or aspect of the software development lifecycle within the Glitchlab environment. These agents automate and streamline complex engineering workflows.

*   **Planner Agent**: Responsible for breaking down high-level tasks into actionable steps and generating detailed execution plans.
*   **Implementer Agent**: Focuses on translating plans into code, making necessary modifications, and ensuring the implementation aligns with the specified requirements.
*   **Debugger Agent**: Identifies and diagnoses issues within the codebase, suggesting and applying fixes to resolve defects and improve system stability.
*   **Security Agent**: Specializes in identifying security vulnerabilities, enforcing security best practices, and ensuring the integrity and confidentiality of the system.
*   **Release Agent**: Manages the release process, including versioning, packaging, and deployment, ensuring a smooth and controlled delivery of software updates.