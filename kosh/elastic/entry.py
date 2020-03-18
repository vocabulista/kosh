from datetime import datetime
from elasticsearch_dsl import Document
from hashlib import sha1
from lxml import etree
from os import path
from re import search
from typing import Any, Dict, List
from unicodedata import normalize

from kosh.utils import logger
from kosh.utils import namespaces as ns


class entry():
    '''
    todo: docs

    '''

    def __init__(self, elex: Dict[str, Any]) -> None:
        '''
        todo: docs
        '''
        self.elex = elex

    def parse(self, file: str) -> List[Document]:
        '''
        todo: docs
        '''
        docs = []
        name = path.basename(file)
        xmap = self.elex.schema.mappings._meta._xpaths

        logger().debug('Parsing file %s/%s', self.elex.uid, name)
        tree = etree.parse(file, etree.XMLParser(remove_blank_text=True))
        for elem in tree.xpath(xmap.root, namespaces=ns()):
            docs += [self.__record(elem)]

        return docs

    def schema(self, *args, **kwargs) -> Document:
        '''
        todo: docs
        '''

        class entry(Document):
            class Index: name = self.elex.uid

        emap = self.elex.schema.mappings.properties
        for i in emap: entry._doc_type.mapping.field(i, emap[i].type)

        return entry(*args, **kwargs)

    def get_id(self, id_xpath):
        for id in id_xpath:
            if isinstance(id, etree._Element) and id.text is not None:
                id = normalize('NFC', id.text)
                return id
            elif isinstance(id, etree._ElementUnicodeResult):
                id = normalize('NFC', id)
                return id
            else:
                id = sha1(elem.encode('utf-8')).hexdigest()
                return id

    def __record(self, root: etree.Element) -> Document:
        '''
        todo: docs
        '''
        elem = etree.tostring(root, encoding='unicode')
        xmap = self.elex.schema.mappings._meta._xpaths

        ##FutureWarning see: https://bugs.python.org/issue38941

        euid = self.get_id(root.xpath(xmap.id, namespaces=ns()))

        item = self.schema(
            meta={'id': euid},
            created=datetime.now(),
            xml=elem
        )

        for prop in xmap.fields:
            for data in root.xpath(xmap.fields[prop], namespaces=ns()):
                if isinstance(data, etree._Element) and data.text is not None:
                    data = normalize('NFC', data.text)
                elif isinstance(data, etree._ElementUnicodeResult):
                    data = normalize('NFC', data)
                else:
                    data = None
                if data is not None:
                    if not search(r'^\[.*\]$', prop):
                        item[prop] = data
                    elif prop[1:-1] in item:
                        item[prop[1:-1]] = [*item[prop[1:-1]], data]
                    else:
                        item[prop[1:-1]] = [data]

        root.clear()
        return item
