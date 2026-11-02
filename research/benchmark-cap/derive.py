"""Reduces data/raw/ to one date,percent file per benchmark in fixtures/, parsed as the node's worker parses them."""

import csv
from datetime import datetime
from decimal import Decimal
from pathlib import Path

import xlrd

HERE = Path(__file__).resolve().parent
RAW = HERE / "data" / "raw"
FIXTURES = HERE / "fixtures"
MISSING = {"", "NA", "N/A", "..", "ND"}


def rows(path):
    """The file's CSV rows; publishers are not all clean UTF-8."""
    with open(path, newline="", encoding="utf-8-sig", errors="replace") as file:
        return list(csv.reader(file))


def named_columns(path, date_column, rate_column, date_format, keep=lambda row: True):
    """Observations under a named header row."""
    table = rows(path)
    head = next(i for i, row in enumerate(table) if {date_column, rate_column} <= {cell.strip() for cell in row})
    columns = [cell.strip() for cell in table[head]]
    day, rate = columns.index(date_column), columns.index(rate_column)
    observations = {}
    for row in table[head + 1 :]:
        if len(row) <= max(day, rate) or not row[day].strip() or not keep(row) or row[rate].strip() in MISSING:
            continue
        observations[datetime.strptime(row[day].strip(), date_format).date()] = Decimal(row[rate].strip())
    return observations


def tona(path):
    """The Bank of Japan's file: rows of a date and the series' value, under a block of metadata."""
    observations = {}
    for row in rows(path):
        if len(row) < 2 or row[1].strip() in MISSING:
            continue
        try:
            day = datetime.strptime(row[0].strip(), "%Y/%m/%d").date()
        except ValueError:
            continue
        observations[day] = Decimal(row[1].strip())
    return observations


def rba_workbook(path):
    """The Reserve Bank of Australia's daily history before 2011, series FIRMMCRID."""
    book = xlrd.open_workbook(path)
    observations = {}
    for sheet in book.sheets():
        head = next((r for r in range(sheet.nrows) if str(sheet.cell_value(r, 0)).strip() == "Series ID"), None)
        if head is None:
            continue
        columns = [str(cell).strip() for cell in sheet.row_values(head)]
        if "FIRMMCRID" not in columns:
            continue
        rate = columns.index("FIRMMCRID")
        for r in range(head + 1, sheet.nrows):
            day, value = sheet.cell(r, 0), sheet.cell(r, rate)
            if day.ctype == xlrd.XL_CELL_DATE and value.ctype == xlrd.XL_CELL_NUMBER:
                year, month, date_of_month, *_ = xlrd.xldate_as_tuple(day.value, book.datemode)
                # The workbook holds binary doubles; its rates have at most four decimals.
                observations[datetime(year, month, date_of_month).date()] = Decimal(f"{value.value:.4f}").normalize()
    return observations


def main():
    """Writes each benchmark's fixture in date order."""
    aonia = rba_workbook(RAW / "aonia-hist.xls")
    aonia.update(named_columns(RAW / "aonia.csv", "Series ID", "FIRMMCRID", "%d-%b-%Y"))
    series = {
        "sofr": named_columns(RAW / "sofr.csv", "Effective Date", "Rate (%)", "%m/%d/%Y", lambda row: row[1] == "SOFR"),
        "estr": named_columns(
            RAW / "estr.csv", "TIME_PERIOD", "OBS_VALUE", "%Y-%m-%d", lambda row: row[0] == "EST.B.EU000A2X2A25.WT"
        ),
        "sonia": named_columns(RAW / "sonia.csv", "DATE", "IUDSOIA", "%d %b %Y"),
        "aonia": aonia,
        "tona": tona(RAW / "tona.csv"),
    }
    FIXTURES.mkdir(exist_ok=True)
    for name, observations in series.items():
        with open(FIXTURES / f"{name}.csv", "w", newline="") as file:
            for day in sorted(observations):
                file.write(f"{day},{format(observations[day], 'f')}\n")
        print(f"{name}: {len(observations)} observations")


if __name__ == "__main__":
    main()
