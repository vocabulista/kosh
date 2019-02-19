from configparser import ConfigParser

from kosh.utils import defaultconfig
from setuptools import find_packages, setup

with open('LICENSE') as f: legal = f.read()
with open('README.md') as f: readme = f.read()

config = ConfigParser()
config.read_dict(defaultconfig())

setup(
  author = 'Francisco Mondaca, Philip Schildkamp',
  author_email = 'f.mondaca@uni-koeln.de, philip.schildkamp@uni-koeln.de',
  description = config.get('info', 'desc'),
  entry_points = { 'console_scripts': ['kosh  =  kosh.kosh:main'] },
  license = legal,
  long_description = readme,
  name = config.get('info', 'name'),
  packages = find_packages(),
  platforms = ['any'],
  url = config.get('info', 'repo'),
  version = '0.0.1'
)
