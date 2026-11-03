"""Downloads each launch benchmark's published history into data/raw/, one file per source."""

import sys
import urllib.request
from pathlib import Path

RAW = Path(__file__).resolve().parent / "data" / "raw"
TIMEOUT_SECONDS = 90
USER_AGENT = "engine-benchmark-cap-study"
SOURCES = {
    "sofr.csv": "https://markets.newyorkfed.org/api/rates/secured/sofr/search.csv?startDate=04/02/2018&endDate=12/31/2026",
    "estr.csv": "https://data-api.ecb.europa.eu/service/data/EST/B.EU000A2X2A25.WT?format=csvdata",
    "sonia.csv": "https://www.bankofengland.co.uk/boeapps/database/_iadb-fromshowcolumns.asp?csv.x=yes&Datefrom=01/Jan/1997&Dateto=31/Dec/2026&SeriesCodes=IUDSOIA&CSVF=TN&UsingCodes=Y&VPD=Y&VFD=N",
    "aonia.csv": "https://www.rba.gov.au/statistics/tables/csv/f1-data.csv",
    "aonia-hist.xls": "https://www.rba.gov.au/statistics/tables/xls-hist/f01dhist.xls",
    "tona.csv": "https://www.stat-search.boj.or.jp/ssi/mtshtml/csv/fm01_d_1_en.csv",
}


def main() -> int:
    """Fetches every source, naming each one that fails rather than stopping at the first."""
    RAW.mkdir(parents=True, exist_ok=True)
    failed = []
    for name, url in SOURCES.items():
        request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
        try:
            with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
                (RAW / name).write_bytes(response.read())
            print(f"fetched {name}")
        except OSError as error:
            failed.append(name)
            print(f"could not fetch {name} from {url}: {error}", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
