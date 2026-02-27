#!/usr/bin/env python3

import argparse
import os
import re
import sys


def run(new_checksum: str = None, new_tag: str = None):
    if new_checksum is None and new_tag is None:
        print('At least one of --checksum or --tag arguments must be provided.', file=sys.stderr)
        sys.exit(1)

    if new_checksum is not None:
        if not new_checksum.isalnum():
            print('Checksum must be alphanumeric.', file=sys.stderr)
            sys.exit(1)

        if not new_checksum.islower():
            print('Checksum must be lowercase.', file=sys.stderr)
            sys.exit(1)

        try:
            int(new_checksum, 16)
        except Exception:
            print('Checksum must be hexadecimal.', file=sys.stderr)
            sys.exit(1)

    if new_tag is not None:
        if new_tag.strip() != new_tag:
            print('Tag must not contain any whitespace.', file=sys.stderr)
            sys.exit(1)

        tag_regex = re.compile("^v?\\d+[.]\\d+[.]\\d+$")
        tag_match = tag_regex.match(new_tag)
        if tag_match is None:
            print('Tag must adhere to x.x.x or vx.x.x major/minor/patch format.', file=sys.stderr)
            sys.exit(1)

    settings = [
        {'variable_name': 'checksum', 'value': new_checksum},
        {'variable_name': 'tag', 'value': new_tag},
    ]

    script_dir = os.path.dirname(os.path.realpath(__file__))
    package_file_path = os.path.join(script_dir, 'Package.swift')

    print(f'Updating: {package_file_path}')

    original_package_file = None
    try:
        with open(package_file_path, 'r') as package_file_handle:
            original_package_file = package_file_handle.read()
    except Exception as e:
        print(f'Failed to read Package.swift file: {e}', file=sys.stderr)
        sys.exit(1)

    package_file = original_package_file
    for current_setting in settings:
        current_variable_name = current_setting['variable_name']
        new_value = current_setting['value']
        if new_value is None:
            continue

        print(f'Setting {current_variable_name}: {new_value}')

        regex = re.compile(f'(let[\\s]+{current_variable_name}[\\s]*=[\\s]*)"([^"]*)"')

        match = regex.search(package_file)
        if match:
            old_value = match.group(2)
            package_file = regex.sub(f'\\1"{new_value}"', package_file)
            print(f'  Changed from: {old_value}')
            print(f'  Changed to:   {new_value}')
        else:
            print(f'  Warning: Could not find {current_variable_name} in Package.swift', file=sys.stderr)

    try:
        with open(package_file_path, "w") as f:
            f.write(package_file)
        print('Successfully updated Package.swift')
    except Exception as e:
        print(f'Failed to write Package.swift file: {e}', file=sys.stderr)
        sys.exit(1)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='Update Package.swift with new checksum and/or tag')
    parser.add_argument('--checksum', type=str, help='new checksum of Paykit.xcframework.zip', required=False, default=None)
    parser.add_argument('--tag', type=str, help='new release tag (e.g., v0.1.0)', required=False, default=None)
    args = parser.parse_args()
    run(new_checksum=args.checksum, new_tag=args.tag)
