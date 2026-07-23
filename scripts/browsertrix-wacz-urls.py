#!/usr/bin/env python3
"""List (and optionally download) the WACZ files behind a public Browsertrix org.

Browsertrix publishes public collections through an unauthenticated API, so we
never need to scrape the web UI or copy presigned URLs out of it by hand:

  1. GET /api/public/orgs/{slug}/collections
       -> the org info + its public collections (id, oid, slug, sizes).
  2. GET /api/orgs/{oid}/collections/{collId}/public/replay.json
       -> resources[]: one entry per WACZ, each with
            path  a presigned S3 URL (range-capable; expires, ~48h)
            hash  sha256 of the file
            size  bytes
            name  original WACZ filename

Presigned URLs expire, but that only matters for the URL itself: once a WACZ is
downloaded to disk it is a normal file and never expires. For rustyweb, download
the WACZs into <home>/archive and `rustyweb index` them.

Examples:
    # List every WACZ URL for an org
    ./browsertrix-wacz-urls.py usgov-archive

    # Just two collections, as a csv
    ./browsertrix-wacz-urls.py usgov-archive -c covid-gov -c coralreef-gov --csv

    # Download into an archive dir, verifying sha256, prefixing files by slug
    ./browsertrix-wacz-urls.py usgov-archive -c covid-gov --download ./archive
"""

import argparse
import hashlib
import json
import os
import sys
import urllib.request

API = "https://app.browsertrix.com"


def get_json(url):
    with urllib.request.urlopen(url) as r:
        return json.load(r)


def list_collections(org_slug):
    """Return (org_dict, [collection_dict, ...]) for a public org slug."""
    data = get_json(f"{API}/api/public/orgs/{org_slug}/collections")
    return data["org"], data["collections"]


def collection_resources(coll):
    """Yield each WACZ resource dict for a collection (from its replay.json)."""
    url = f"{API}/api/orgs/{coll['oid']}/collections/{coll['id']}/public/replay.json"
    return get_json(url).get("resources", [])


def iter_waczs(org_slug, only_slugs=None):
    """Yield (collection, resource) pairs, optionally filtered to some slugs."""
    _, colls = list_collections(org_slug)
    if only_slugs:
        wanted = set(only_slugs)
        colls = [c for c in colls if c["slug"] in wanted]
        missing = wanted - {c["slug"] for c in colls}
        if missing:
            sys.exit(f"error: no such collection slug(s): {', '.join(sorted(missing))}")
    for c in colls:
        for r in collection_resources(c):
            yield c, r


def download(resource, dest_dir, coll_slug):
    """Stream a WACZ to dest_dir, verifying sha256. Returns the output path."""
    os.makedirs(dest_dir, exist_ok=True)
    base = os.path.basename(resource["name"])
    if not base.endswith(".wacz"):
        base += ".wacz"
    out = os.path.join(dest_dir, f"{coll_slug}__{base}")
    h = hashlib.sha256()
    with urllib.request.urlopen(resource["path"]) as r, open(out, "wb") as f:
        for chunk in iter(lambda: r.read(1 << 20), b""):
            f.write(chunk)
            h.update(chunk)
    got = h.hexdigest()
    if got != resource["hash"]:
        os.remove(out)
        sys.exit(f"error: sha256 mismatch for {out} ({got} != {resource['hash']})")
    return out


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("org_slug", help="public org slug, e.g. usgov-archive")
    p.add_argument("-c", "--collection", action="append", dest="collections",
                   metavar="SLUG", help="limit to this collection slug (repeatable)")
    p.add_argument("--csv", action="store_true",
                   help="print a human-readable table as csv")
    p.add_argument("--download", metavar="DIR",
                   help="download each WACZ into DIR, verifying sha256")
    args = p.parse_args(argv)

    if args.download:
        for coll, res in iter_waczs(args.org_slug, args.collections):
            print(f"downloading {res['name']} ({res['size'] / 1e6:.1f} MB) "
                  f"from {coll['slug']} ...", file=sys.stderr)
            out = download(res, args.download, coll["slug"])
            print(f"  ok  {out}", file=sys.stderr)
        return

    if args.csv:
        print("slug,size,hash,path")

    for coll, res in iter_waczs(args.org_slug, args.collections):
        if args.csv:
            print(f"{coll['slug']},{res['size']},{res['hash']},{res['path']}")
        else:
            print(res["path"])


if __name__ == "__main__":
    main()
