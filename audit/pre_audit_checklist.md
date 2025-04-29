## Audit Preparation Checklist

**Identify the Audit Scope:** [x]  
- Determine the specific areas of the codebase that will be audited. [x]  
- Identify any dependencies that should be included in the audit. [x]

**Gather Documentation:** [x]  
- Collect all relevant documents (e.g., whitepapers, technical specifications, architectural diagrams). [x]  
- Ensure documentation is comprehensive, up-to-date, and accessible to the auditors. [x]

**Provide Access to the Codebase:** [x]  
- Grant access to the complete smart contract codebase, including all relevant files and dependencies. [x]  
- Host the code in a secure repository to allow safe auditor access. [x]

**Share Deployment Information:** [x]  
- Provide details of any deployed contracts, including:  
  - Contract addresses [x]  
  - Transaction hashes [x]  
  - Information on available dApps for interacting with the components [x]  
- If deployed on a testnet, share the relevant deployment details. [x]

**Share Test Cases:** [x]  
- Provide any internal test cases and security assessment results. [x]  
- Include any identified issues from in-house testing to help focus the audit on critical areas. [x]

**Communicate with the Auditor:** [x]  
- Establish a clear communication channel with the auditor. [x]  
- Appoint a knowledgeable contact person to act as the liaison. [x]  
- Raise any areas of extra concern or additional information needed during the preparation phase. [x]

---

## Audit Requirements Checklist

In addition to the preparation tasks, ensure that the following technical requirements are met prior to the audit:

**Compilation and Testing:** [x]  
- Ensure contracts compile and pass all tests using `cargo test`. [x]  
- Run `cargo fmt` to format the Rust code for consistency and improved readability. [x]  
- Address as many issues as possible reported by `cargo clippy`. [x]

**Test Coverage:** [x]  
- Achieve a minimum of 40% test coverage as reported by `cargo tarpaulin`. [x]

**Dependency Security:** [x]  
- Run `cargo audit`. [x]  
- Ensure that no problematic dependencies are flagged to avoid potential supply chain attacks. [x]

**Code Freeze:** [x]  
- Ensure the code freeze hash remains unchanged during the audit. [x]  
- Avoid any new commits or alterations while the audit is in progress. [x]

**Code Hygiene:** [x]  
- Remove all unreachable code. [x]  
- Address or remove any leftover `//TODO` comments. [x]  
- Eliminate non-relevant file templates or leftover boilerplate files. [x]

