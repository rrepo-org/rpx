# Repository Management Follow-Up

Add commands to manage every supported repository type, not only additional package repositories.

The command surface should cover:

- The base repository
- Additional Rrepo or CRAN-like repositories
- Git repositories and their references or subdirectories

Design a repository-spec format that can represent each type consistently in command arguments and output. Consider how the format distinguishes repository kinds, URLs, Git references, subdirectories, and repository-specific options while remaining convenient for interactive CLI use.

Repository add, remove, and list commands should use the same format and round-trip repository configuration without losing type-specific information.
