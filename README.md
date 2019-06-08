# Kosh - APIs for Dictionaries

## How to run Kosh (Linux, OSX) with data from [kosh_data](https://github.com/cceh/kosh_data):

1. [elasticsearch](https://www.elastic.co/downloads/elasticsearch)  must be running on your computer
2. `git clone https://github.com/cceh/kosh`
3. `git clone https://github.com/cceh/kosh_data`
4. `cd kosh`
5. `make`
5. start kosh:

    on Linux: 
     
    `kosh --log_level DEBUG --data_root ../kosh_data --data_host localhost`
    
    on OSX:
     
    `kosh --log_level DEBUG --data_root ../kosh_data --data_host localhost --data_sync off`

## Query examples:

For testing your local instance, replace `http://kosh.uni-koeln.de/` with `http://localhost:5000/` in the URLs of the next examples.

### Wörterbuch der Kölner Mundart - Fritz Hönig (Kölsch-Deutsch)

* REST: <http://kosh.uni-koeln.de/api/hoenig/restful>

* GraphQL: <http://kosh.uni-koeln.de/api/hoenig/graphql>
```
{
  entries(queryType: prefix, query: "scha", field:lemma_ksh ) {
    id 
    lemmaKsh
    translationDeu
  }
}
```

### TuniCo - [A Dictionary of Tunis Arabic](https://arche.acdh.oeaw.ac.at/browser/oeaw_detail/id.acdh.oeaw.ac.at/uuid/175b8cdf-5d04-f4d3-a778-67910aa8fd37)


* REST: <http://kosh.uni-koeln.de/api/tunico/restful>

* GraphQL: <http://kosh.uni-koeln.de/api/tunico/graphql>

```
{
  entries(queryType: regexp, query: ".*ung", field: trans_de) {
    id 
    lemma	 
    transEn
    transDe
  }
}
```

### [Freedict German - Dutch](https://github.com/freedict/fd-dictionaries/tree/master/deu-nld)

* REST: <http://kosh.uni-koeln.de/api/freedict_deu_nld/restful>
* GraphQL:  <http://kosh.uni-koeln.de/api/freedict_deu_nld/graphql>
```
{
  entries(queryType: term, query: "lieben", field: lemma_deu) {
    id 
    translationNld
  }
}
```


### [Freedict Breton - French](https://github.com/freedict/fd-dictionaries/tree/master/bre-fra)

* REST: <http://kosh.uni-koeln.de/api/freedict_bre_fra/restful>

* GraphQL:  <http://kosh.uni-koeln.de/api/freedict_bre_fra/graphql>
```
{
  entries(queryType: wildcard, query: "*eler", field: lemma_bre) {
    id 
    lemmaBre
    translationFra
  }
}
```

### [Hiztegi Batu Oinarriduna](http://www.euskaltzaindia.eus/dok/eaeb/hiztegibatua/hiztegibatua.xml) (Basque)

* REST: <http://kosh.uni-koeln.de/api/hiztegibatua/restful>

* GraphQL:  <http://kosh.uni-koeln.de/api/hiztegibatua/graphql>
```
{
  entries(queryType: wildcard, query: "*etsu", field: lemma) {
    id 
    lemma
    sensePos
    xml
  }
}
```



### Diccionario Geográfico-Histórico de las Indias Occidentales ó América (1786-1789) de Antonio de Alcedo

* REST: <http://kosh.uni-koeln.de/api/de_alcedo/restful>
* GraphQL:  <http://kosh.uni-koeln.de/api/de_alcedo/graphql>
```
{
  entries(queryType: wildcard, query: "*HUE", field: lemma) {
    id 
    lemma
    xml
  }
}
```
