# pg_rebac: PGXS packaging for the pure-SQL install, and the PGXN distribution.
#   make               generates dist/pg_rebac--$(REBAC_VERSION).sql from sql/rebac.sql
#   make install       copies it into $(pg_config --sharedir)/extension (PGXS DATA); there is NO control file,
#                      so it is applied with `psql -f`, not CREATE EXTENSION (see docs / release notes)
#   make dist          pg_rebac-$(REBAC_VERSION).zip for PGXN, from `git archive` of HEAD plus the generated SQL
# The release's version, from META.json. Not VERSION: PGXS sets that to the Postgres version.
REBAC_VERSION := $(shell sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' META.json | head -1)
SQLFILE = dist/pg_rebac--$(REBAC_VERSION).sql
DATA    = $(SQLFILE)
EXTRA_CLEAN = dist

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
-include $(PGXS)

all: $(SQLFILE)

$(SQLFILE): sql/rebac.sql scripts/release-sql.sh META.json
	scripts/release-sql.sh $(REBAC_VERSION) $@

.PHONY: dist
dist: $(SQLFILE)
	rm -rf dist/pg_rebac-$(REBAC_VERSION) dist/pg_rebac-$(REBAC_VERSION).zip
	mkdir -p dist/pg_rebac-$(REBAC_VERSION)
	git archive --format=tar --prefix=pg_rebac-$(REBAC_VERSION)/ HEAD | tar -x -C dist
	cp $(SQLFILE) dist/pg_rebac-$(REBAC_VERSION)/
	cd dist && zip -qr pg_rebac-$(REBAC_VERSION).zip pg_rebac-$(REBAC_VERSION) && rm -rf pg_rebac-$(REBAC_VERSION)
	@echo dist/pg_rebac-$(REBAC_VERSION).zip
